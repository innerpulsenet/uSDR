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

/// Audio band for AM voice. Wider than narrowband FM's 3 kHz: aviation and
/// broadcast AM carry useful content to 4 kHz plus.
pub const AM_VOICE_LOWPASS_HZ: f32 = 4_000.0;

/// Corner of the DC blocker after the detector: under the lowest voice and
/// broadcast bass worth keeping, over the fading and gain-loop wobble.
const DC_BLOCK_HZ: f32 = 60.0;

pub struct AmDemod {
    prev: Complex32,
    /// Mean-level AGC state: the smoothed carrier-plus-trough level, and
    /// the first of the two smoothing stages it comes out of.
    agc: f32,
    agc_pre: f32,
    /// AGC time constant in samples, split evenly over the two stages.
    agc_tau: f32,
    fs: f32,
    voice_lp: Cascade,
    dc_block: Cascade,
}

impl AmDemod {
    pub fn new(fs: f64) -> Self {
        Self::with_audio_lowpass(fs, AM_VOICE_LOWPASS_HZ)
    }

    /// With the audio band ending at `audio_hz`: half the receive filter's
    /// width, since AM carries the same audio in both sidebands, so a wider
    /// filter on broadcast AM brings its treble back.
    pub fn with_audio_lowpass(fs: f64, audio_hz: f32) -> Self {
        let fs32 = fs as f32;
        Self {
            prev: Complex32::new(0.0, 0.0),
            // ~50 ms: fast enough to track fading on HF, slow enough that
            // 100% modulated speech does not pump the gain at syllable rate.
            agc: 1.0,
            agc_pre: 1.0,
            agc_tau: crate::agc::AgcSpeed::Medium.am_tau_s() * fs32,
            fs: fs32,
            // True Butterworth: flat to the corner, −3 dB there. Identical
            // Q = 0.707 sections were −3 dB about a fifth of the way below it.
            voice_lp: Cascade::butterworth_lowpass(audio_hz.min(fs32 * 0.45), fs32, 2),
            // Removes the detected carrier (DC) without touching the
            // modulation sidebands above it. This was a one-pole whose time
            // constant was 0.2 ms, a high-pass at 800 Hz that left 300 Hz
            // 7 dB and 100 Hz 16 dB under 1 kHz: thin, bass-less AM.
            dc_block: Cascade::butterworth_highpass(DC_BLOCK_HZ, fs32, 2),
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
            // Two stages of half the time constant each: about as quick to
            // follow a fade as one, but the audio's own ripple on the level,
            // which multiplies into the audio as intermodulation, is cut by
            // a second pole (100 Hz against 1 kHz on fast AGC: -34 -> -45 dB).
            let k = 2.0 / self.agc_tau.max(2.0);
            self.agc_pre += (mag - self.agc_pre) * k;
            self.agc += (self.agc_pre - self.agc) * k;
            // Normalise, then remove the carrier: (mag - agc) is the
            // modulation centred on zero, scaled so 100% modulation peaks
            // near ±1.0 when the AGC has settled onto the carrier level.
            let audio = if self.agc > 1e-6 {
                (mag - self.agc) / self.agc
            } else {
                0.0
            };
            // Extra DC removal mops up AGC ripple before the audio filter.
            let dc_free = self.dc_block.process(audio);
            out.push(self.voice_lp.process(dc_free).clamp(-1.5, 1.5));
        }
    }

    pub fn set_agc(&mut self, speed: crate::agc::AgcSpeed) {
        self.agc_tau = speed.am_tau_s() * self.fs;
    }

    pub fn reset(&mut self) {
        self.prev = Complex32::new(0.0, 0.0);
        self.agc = 1.0;
        self.agc_pre = 1.0;
        self.voice_lp.reset();
        self.dc_block.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    const FS: f64 = 48_000.0;

    /// A carrier of amplitude `a` modulated by `tones` (hz, depth).
    fn am(n: usize, a: f32, tones: &[(f32, f32)]) -> Vec<Complex32> {
        (0..n)
            .map(|i| {
                let t = i as f64 / FS;
                let m: f32 = tones.iter().map(|&(hz, d)| d * (TAU as f64 * hz as f64 * t).sin() as f32).sum();
                // A fixed carrier phase: the envelope is all that matters.
                Complex32::from_polar(a * (1.0 + m), 0.3)
            })
            .collect()
    }

    fn amp_at(x: &[f32], hz: f32) -> f32 {
        let w = TAU as f64 * hz as f64 / FS;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, &s) in x.iter().enumerate() {
            re += s as f64 * (w * i as f64).cos();
            im += s as f64 * (w * i as f64).sin();
        }
        (2.0 * (re * re + im * im).sqrt() / x.len() as f64) as f32
    }

    fn demod(x: &[Complex32]) -> Vec<f32> {
        demod_at(x, crate::agc::AgcSpeed::Medium)
    }

    fn demod_at(x: &[Complex32], speed: crate::agc::AgcSpeed) -> Vec<f32> {
        let mut d = AmDemod::new(FS);
        d.set_agc(speed);
        let mut out = Vec::new();
        let mut all = Vec::new();
        for c in x.chunks(480) {
            d.process(c, &mut out);
            all.extend_from_slice(&out);
        }
        all
    }

    /// Voice frequencies come out at one level: the detector's DC blocker
    /// must not reach up into the voice band.
    #[test]
    fn voice_band_is_flat() {
        let n = (FS * 2.0) as usize;
        let mut levels = Vec::new();
        for hz in [100.0f32, 200.0, 300.0, 1_000.0, 2_500.0] {
            let y = demod(&am(n, 0.1, &[(hz, 0.5)]));
            let a = amp_at(&y[n / 2..], hz);
            levels.push((hz, 20.0 * (a / 0.5).log10()));
        }
        eprintln!("response: {levels:.1?}");
        let at1k = levels[3].1;
        for (hz, db) in &levels {
            if *hz < 2_500.0 {
                assert!((db - at1k).abs() < 1.0, "{hz} Hz sits {:.1} dB off 1 kHz", db - at1k);
            }
        }
    }

    /// The gain loop must not follow the audio: a loop that does multiplies
    /// the bass into everything else. Two tones at 90% total modulation; the
    /// sum and difference products stay far under them.
    #[test]
    fn agc_ripple_does_not_intermodulate() {
        use crate::agc::AgcSpeed;
        let n = (FS * 2.0) as usize;
        for speed in [AgcSpeed::Fast, AgcSpeed::Medium, AgcSpeed::Slow] {
            for bass in [100.0f32, 150.0] {
                let y = demod_at(&am(n, 0.1, &[(bass, 0.45), (1_000.0, 0.45)]), speed);
                let tail = &y[n / 2..];
                let wanted = amp_at(tail, 1_000.0);
                let imd = amp_at(tail, 1_000.0 - bass).max(amp_at(tail, 1_000.0 + bass));
                let db = 20.0 * (imd / wanted).log10();
                eprintln!("{speed:?}: IMD at 1 kHz ± {bass} Hz: {db:.1} dB");
                assert!(db < -40.0, "{speed:?}: intermodulation {db:.1} dB under the tone");
            }
        }
    }
}
