//! Second-order sections, for the audio-rate filtering the DSP crate does not
//! cover.
//!
//! `scannerd-dsp` deals in windowed-sinc FIRs sized for MHz-wide spans. At an
//! 16 kHz audio rate the shapes needed here — splitting sub-audible CTCSS from
//! voice — are a few hundred Hz wide, where an FIR long enough to be selective
//! would cost more than the whole demodulator. Biquads are the right tool at
//! this rate and this selectivity.

use std::f32::consts::TAU;

/// One biquad section in transposed direct form II.
#[derive(Clone, Debug)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    /// RBJ cookbook low-pass.
    pub fn lowpass(fc: f32, fs: f32, q: f32) -> Self {
        let (cs, alpha, a0_inv, w0) = common(fc, fs, q);
        let _ = w0;
        let b0 = (1.0 - cs) / 2.0;
        Self::normalised(b0, 1.0 - cs, b0, -2.0 * cs, 1.0 - alpha, a0_inv)
    }

    /// RBJ cookbook high-pass.
    pub fn highpass(fc: f32, fs: f32, q: f32) -> Self {
        let (cs, alpha, a0_inv, _) = common(fc, fs, q);
        let b0 = (1.0 + cs) / 2.0;
        Self::normalised(b0, -(1.0 + cs), b0, -2.0 * cs, 1.0 - alpha, a0_inv)
    }

    fn normalised(b0: f32, b1: f32, b2: f32, a1: f32, a2: f32, a0_inv: f32) -> Self {
        Self {
            b0: b0 * a0_inv,
            b1: b1 * a0_inv,
            b2: b2 * a0_inv,
            a1: a1 * a0_inv,
            a2: a2 * a0_inv,
            z1: 0.0,
            z2: 0.0,
        }
    }

    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }

    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }
}

fn common(fc: f32, fs: f32, q: f32) -> (f32, f32, f32, f32) {
    let w0 = TAU * fc.clamp(1.0, fs * 0.49) / fs;
    let (sn, cs) = w0.sin_cos();
    let alpha = sn / (2.0 * q.max(0.05));
    (cs, alpha, 1.0 / (1.0 + alpha), w0)
}

/// A cascade of identical sections, for when one is not steep enough.
#[derive(Clone, Debug)]
pub struct Cascade {
    sections: Vec<Biquad>,
}

impl Cascade {
    pub fn new(sections: Vec<Biquad>) -> Self {
        Self { sections }
    }

    /// Butterworth-ish high-pass of `n` sections, 12 dB/octave each.
    ///
    /// Two sections is the working figure for stripping CTCSS: a tone at
    /// 192.8 Hz sits 0.64 octaves below a 300 Hz corner, which one section
    /// barely touches. Tones are also transmitted at roughly a fifth of voice
    /// deviation, so 24 dB/octave here puts them far enough down to stay out of
    /// the audio without cutting into speech.
    pub fn highpass(fc: f32, fs: f32, n: usize) -> Self {
        Self::new((0..n).map(|_| Biquad::highpass(fc, fs, 0.707)).collect())
    }

    pub fn lowpass(fc: f32, fs: f32, n: usize) -> Self {
        Self::new((0..n).map(|_| Biquad::lowpass(fc, fs, 0.707)).collect())
    }

    pub fn process(&mut self, x: f32) -> f32 {
        self.sections.iter_mut().fold(x, |acc, s| s.process(acc))
    }

    pub fn reset(&mut self) {
        for s in &mut self.sections {
            s.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a filter with a tone and report its steady-state amplitude in dB.
    fn response_db(mut f: Cascade, freq: f32, fs: f32) -> f32 {
        let n = (fs * 0.5) as usize;
        let mut peak = 0.0f32;
        for i in 0..n {
            let x = (TAU * freq * i as f32 / fs).sin();
            let y = f.process(x);
            // Ignore the settling transient.
            if i > n / 2 {
                peak = peak.max(y.abs());
            }
        }
        20.0 * (peak + 1e-12).log10()
    }

    #[test]
    fn the_voice_band_passes_and_ctcss_does_not() {
        let fs = 16_000.0;
        let pass = response_db(Cascade::highpass(300.0, fs, 2), 1000.0, fs);
        let tone = response_db(Cascade::highpass(300.0, fs, 2), 192.8, fs);
        assert!(pass > -1.0, "1 kHz speech should pass, got {pass:.1} dB");
        assert!(
            tone < -15.0,
            "192.8 Hz CTCSS should be well down, got {tone:.1} dB"
        );
    }

    /// The complementary path: CTCSS detection needs voice kept out.
    #[test]
    fn the_ctcss_path_keeps_the_tone_and_rejects_speech() {
        let fs = 16_000.0;
        let tone = response_db(Cascade::lowpass(250.0, fs, 2), 100.0, fs);
        let voice = response_db(Cascade::lowpass(250.0, fs, 2), 1000.0, fs);
        assert!(tone > -3.0, "100 Hz tone should pass, got {tone:.1} dB");
        assert!(
            voice < -20.0,
            "1 kHz speech should be down, got {voice:.1} dB"
        );
    }

    #[test]
    fn a_reset_filter_starts_from_silence() {
        let mut f = Cascade::highpass(300.0, 16_000.0, 2);
        for _ in 0..100 {
            f.process(1.0);
        }
        f.reset();
        assert_eq!(f.process(0.0), 0.0);
    }
}
