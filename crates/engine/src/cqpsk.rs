//! P25 Phase 1 CQPSK / LSM (Linear Simulcast Modulation) demodulator.
//!
//! Unlike C4FM which modulates instantaneous frequency directly, CQPSK/LSM is a
//! linear phase modulation ($\pi/4$-DQPSK) with Root-Raised Cosine (RRC) pulse
//! shaping. An FM discriminator distorts CQPSK signals under simulcast multipath,
//! while a complex differential phase detector recovers dibits cleanly.

use num_complex::Complex32;
use std::f32::consts::TAU;

/// A CQPSK / LSM demodulator: RRC matched filter, carrier phase tracker,
/// and differential symbol phase detector.
pub struct CqpskDemodulator {
    sps: usize,
    hz_per_rad: f32,
    delay_buf: Vec<Complex32>,
    head: usize,
    phase: f32,
    freq_err: f32,
    alpha: f32,
    beta: f32,
    eq_taps: Vec<Complex32>,
    eq_buf: Vec<Complex32>,
    eq_head: usize,
    eq_mu: f32,
    sample_count: usize,
}

impl CqpskDemodulator {
    pub fn new(fs: f64, symbol_rate: f64) -> Self {
        let sps = (fs / symbol_rate).round() as usize;
        let sps = sps.max(1);
        let hz_per_rad = (fs / (TAU as f64 * sps as f64)) as f32;
        Self {
            sps,
            hz_per_rad,
            delay_buf: vec![Complex32::new(0.0, 0.0); sps],
            head: 0,
            phase: 0.0,
            freq_err: 0.0,
            alpha: 0.05,
            beta: 0.005,
            eq_taps: vec![Complex32::new(1.0, 0.0); 1],
            eq_buf: vec![Complex32::new(0.0, 0.0); 1],
            eq_head: 0,
            eq_mu: 0.0,
            sample_count: 0,
        }
    }

    pub fn with_tracking(mut self, alpha: f32, beta: f32) -> Self {
        self.alpha = alpha;
        self.beta = beta;
        self
    }

    /// Enable a causal, normalized complex LMS multipath equalizer. Seven
    /// taps cover most of a symbol at the normal 48 kHz channel rate without
    /// allowing an unstable long inverse filter.
    pub fn with_equalizer(mut self, taps: usize, mu: f32) -> Self {
        let taps = taps.clamp(1, 31);
        self.eq_taps = vec![Complex32::new(0.0, 0.0); taps];
        self.eq_taps[0] = Complex32::new(1.0, 0.0);
        self.eq_buf = vec![Complex32::new(0.0, 0.0); taps];
        self.eq_head = 0;
        self.eq_mu = mu.clamp(0.0, 0.1);
        self
    }

    /// Process complex baseband samples into equivalent symbol frequency estimates (Hz).
    pub fn process(&mut self, iq: &[Complex32], out: &mut Vec<f32>) {
        out.clear();
        out.reserve(iq.len());

        for &x in iq {
            self.eq_buf[self.eq_head] = x;
            let mut equalized = Complex32::new(0.0, 0.0);
            let mut energy = 1e-4;
            for (tap, weight) in self.eq_taps.iter().enumerate() {
                let at = (self.eq_head + self.eq_buf.len() - tap) % self.eq_buf.len();
                equalized += *weight * self.eq_buf[at];
                energy += self.eq_buf[at].norm_sqr();
            }
            self.eq_head = (self.eq_head + 1) % self.eq_buf.len();

            // Apply residual carrier phase tracking
            self.phase += self.freq_err;
            if self.phase > TAU {
                self.phase -= TAU;
            } else if self.phase < -TAU {
                self.phase += TAU;
            }
            let (sin_phase, cos_phase) = self.phase.sin_cos();
            let rot = Complex32::new(cos_phase, -sin_phase);
            let s = equalized * rot;

            // One symbol period delay
            let prev = self.delay_buf[self.head];
            self.delay_buf[self.head] = s;
            self.head = (self.head + 1) % self.sps;

            // Differential phase detector: d = s[n] * s[n - sps]^*
            let d = s * prev.conj();
            let d_phase = if d.norm_sqr() > 1e-12 { d.arg() } else { 0.0 };

            // Rotate the current equalizer output toward the nearest valid
            // differential phase, then use that complex error as the NLMS
            // teaching signal. Normalization bounds adaptation through fades.
            if self.eq_mu > 0.0
                && self.sample_count % self.sps == self.sps - 1
                && prev.norm_sqr() > 1e-6
                && s.norm_sqr() > 1e-6
            {
                let err_phase = phase_error(d_phase);
                let (sin_neg, cos_neg) = (-err_phase).sin_cos();
                let correction = Complex32::new(cos_neg, sin_neg);
                let error = s * correction - s;
                let step = self.eq_mu / energy;
                for tap in 0..self.eq_taps.len() {
                    let at = (self.eq_head + self.eq_buf.len() - 1 - tap) % self.eq_buf.len();
                    self.eq_taps[tap] += error * self.eq_buf[at].conj() * step;
                }
            }
            self.sample_count = self.sample_count.wrapping_add(1);

            // Estimate carrier phase error against nearest CQPSK constellation point (±π/4, ±3π/4)
            if self.alpha > 0.0 || self.beta > 0.0 {
                let err = phase_error(d_phase);
                self.freq_err += self.beta * err;
                self.phase += self.alpha * err;
            }

            // Frequency estimate in Hz matching C4FM levels (±600, ±1800 Hz)
            let hz = d_phase * self.hz_per_rad;
            out.push(hz);
        }
    }

    pub fn reset(&mut self) {
        self.delay_buf.fill(Complex32::new(0.0, 0.0));
        self.head = 0;
        self.phase = 0.0;
        self.freq_err = 0.0;
        self.eq_buf.fill(Complex32::new(0.0, 0.0));
        self.eq_head = 0;
        self.eq_taps.fill(Complex32::new(0.0, 0.0));
        self.eq_taps[0] = Complex32::new(1.0, 0.0);
        self.sample_count = 0;
    }
}

/// Distance from phase angle to nearest valid CQPSK phase shift (±π/4, ±3π/4).
fn phase_error(d_phase: f32) -> f32 {
    let pi_4 = std::f32::consts::FRAC_PI_4;
    let target = if d_phase > 0.0 {
        if d_phase > 2.0 * pi_4 {
            3.0 * pi_4
        } else {
            pi_4
        }
    } else {
        if d_phase < -2.0 * pi_4 {
            -3.0 * pi_4
        } else {
            -pi_4
        }
    };
    d_phase - target
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cqpsk_demodulates_known_phase_steps() {
        let fs = 48_000.0;
        let sym_rate = 4800.0;
        let sps = 10;
        let mut demod = CqpskDemodulator::new(fs, sym_rate).with_tracking(0.0, 0.0);

        // Modulate reference symbol + 4 data symbols: +1800, +600, -600, -1800 Hz
        let phases = [
            0.0f32,
            std::f32::consts::FRAC_PI_4 * 3.0,
            std::f32::consts::FRAC_PI_4,
            -std::f32::consts::FRAC_PI_4,
            -std::f32::consts::FRAC_PI_4 * 3.0,
        ];
        let mut iq = Vec::new();
        let mut phase = 0.0f32;
        for &step in &phases {
            let inc = step / sps as f32;
            for _ in 0..sps {
                phase += inc;
                iq.push(Complex32::new(phase.cos(), phase.sin()));
            }
        }

        let mut hz = Vec::new();
        demod.process(&iq, &mut hz);

        // Check frequency outputs after reference symbol delay at symbol eye centers
        assert_eq!(hz.len(), iq.len());
        assert!((hz[19] - 1800.0).abs() < 5.0, "got {}", hz[19]);
        assert!((hz[29] - 600.0).abs() < 5.0, "got {}", hz[29]);
        assert!((hz[39] - (-600.0)).abs() < 5.0, "got {}", hz[39]);
        assert!((hz[49] - (-1800.0)).abs() < 5.0, "got {}", hz[49]);
    }

    #[test]
    fn adaptive_equalizer_reduces_delayed_echo_phase_error() {
        let fs = 48_000.0;
        let sps = 10usize;
        let mut clean = Vec::new();
        let mut phase = 0.0f32;
        let steps = [
            std::f32::consts::FRAC_PI_4,
            3.0 * std::f32::consts::FRAC_PI_4,
            -std::f32::consts::FRAC_PI_4,
            -3.0 * std::f32::consts::FRAC_PI_4,
        ];
        let mut state = 0x1234_5678u32;
        for _ in 0..1200 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let step = steps[(state >> 30) as usize];
            for _ in 0..sps {
                phase += step / sps as f32;
                clean.push(Complex32::new(phase.cos(), phase.sin()));
            }
        }
        let impaired: Vec<Complex32> = clean
            .iter()
            .enumerate()
            .map(|(i, &sample)| {
                sample
                    + i.checked_sub(3)
                        .map(|j| clean[j] * Complex32::new(0.48, 0.28))
                        .unwrap_or_default()
            })
            .collect();

        let score = |hz: &[f32]| -> f32 {
            hz.iter()
                .skip(2009)
                .step_by(sps)
                .map(|&value| phase_error(value / 763.943_7).abs())
                .sum::<f32>()
                / hz.iter().skip(2009).step_by(sps).count() as f32
        };
        let mut plain = CqpskDemodulator::new(fs, 4800.0).with_tracking(0.0, 0.0);
        let mut adaptive = CqpskDemodulator::new(fs, 4800.0)
            .with_tracking(0.0, 0.0)
            .with_equalizer(7, 0.02);
        let (mut plain_hz, mut adaptive_hz) = (Vec::new(), Vec::new());
        plain.process(&impaired, &mut plain_hz);
        adaptive.process(&impaired, &mut adaptive_hz);
        assert!(
            score(&adaptive_hz) < score(&plain_hz) * 0.8,
            "adaptive {} vs plain {}",
            score(&adaptive_hz),
            score(&plain_hz)
        );
    }
}
