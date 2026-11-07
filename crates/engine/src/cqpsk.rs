//! P25 Phase 1 CQPSK / LSM (Linear Simulcast Modulation) demodulator.
//!
//! Unlike C4FM which modulates instantaneous frequency directly, CQPSK/LSM is a
//! linear phase modulation ($\pi/4$-DQPSK) with Root-Raised Cosine (RRC) pulse
//! shaping. An FM discriminator distorts CQPSK signals under simulcast multipath,
//! while a complex differential phase detector recovers dibits cleanly.
//!
//! The chain is RRC matched filter → optional NLMS equalizer → one-symbol
//! differential detector. There is no carrier loop: the differential
//! detector is blind to carrier phase, and a residual frequency offset only
//! shifts every output by the same few Hz, which the frame detector's sync
//! fit removes per frame and the receiver's AFC feeds back to the mixer.
//! A decision-directed loop here used to steer on every sample, including
//! the nine in ten that sit mid-transition and carry no decision; it walked
//! the phase away under noise and turned 20 dB Es/N0 into a 73 % symbol
//! error rate.

use crate::p25::rrc_taps;
use num_complex::Complex32;
use std::f32::consts::TAU;

/// A CQPSK / LSM demodulator: RRC matched filter, optional multipath
/// equalizer, and differential symbol phase detector.
pub struct CqpskDemodulator {
    sps: usize,
    hz_per_rad: f32,
    /// Root-raised-cosine receive filter (α = 0.2, eight symbols), matched
    /// to the LSM transmit filter. Without it the detector saw the whole
    /// channel filter's noise — several times the symbol bandwidth.
    mf_taps: Vec<f32>,
    mf_buf: Vec<Complex32>,
    mf_head: usize,
    delay_buf: Vec<Complex32>,
    head: usize,
    eq_taps: Vec<Complex32>,
    eq_buf: Vec<Complex32>,
    eq_head: usize,
    eq_mu: f32,
    /// Leaky mean of the matched filter's output power at each of the
    /// `sps` sample phases. The symbol instants are where it peaks, and the
    /// equalizer adapts only there: a decision taught mid-transition is a
    /// wrong decision.
    phase_power: Vec<f32>,
    /// Mean decision error at the strobes: a carrier offset turns every
    /// symbol by the same angle, which no FIR can remove, so the equalizer
    /// is taught only the part of the error that varies.
    err_bias: f32,
    sample_count: usize,
}

impl CqpskDemodulator {
    pub fn new(fs: f64, symbol_rate: f64) -> Self {
        let sps = (fs / symbol_rate).round() as usize;
        let sps = sps.max(1);
        let hz_per_rad = (fs / (TAU as f64 * sps as f64)) as f32;
        let mf_taps = rrc_taps(sps, 8, 0.2);
        // Unit gain at DC so the equalizer's step and the detector see the
        // input's own scale.
        let dc: f32 = mf_taps.iter().sum();
        let mf_taps: Vec<f32> = mf_taps.iter().map(|t| t / dc).collect();
        Self {
            sps,
            hz_per_rad,
            mf_buf: vec![Complex32::new(0.0, 0.0); mf_taps.len()],
            mf_taps,
            mf_head: 0,
            delay_buf: vec![Complex32::new(0.0, 0.0); sps],
            head: 0,
            eq_taps: vec![Complex32::new(1.0, 0.0); 1],
            eq_buf: vec![Complex32::new(0.0, 0.0); 1],
            eq_head: 0,
            eq_mu: 0.0,
            phase_power: vec![0.0; sps],
            err_bias: 0.0,
            sample_count: 0,
        }
    }

    /// Enable a causal, normalized complex LMS multipath equalizer, spaced
    /// one sample apart: seven taps span 0.7 of a symbol at 48 kHz, short
    /// of a true simulcast delay spread but enough to take a one-symbol
    /// echo from ~6 % symbol errors to under 1 % at high SNR. It adapts
    /// only at the symbol instants and only on confident decisions; a
    /// fractionally spaced design with real timing recovery is the next
    /// step if simulcast sites need more.
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
        let ntaps = self.mf_taps.len();

        for &x in iq {
            // Matched filter.
            self.mf_buf[self.mf_head] = x;
            self.mf_head = (self.mf_head + 1) % ntaps;
            let mut filtered = Complex32::new(0.0, 0.0);
            for (k, &t) in self.mf_taps.iter().enumerate() {
                filtered += self.mf_buf[(self.mf_head + k) % ntaps] * t;
            }

            self.eq_buf[self.eq_head] = filtered;
            let mut s = Complex32::new(0.0, 0.0);
            let mut energy = 1e-4;
            for (tap, weight) in self.eq_taps.iter().enumerate() {
                let at = (self.eq_head + self.eq_buf.len() - tap) % self.eq_buf.len();
                s += *weight * self.eq_buf[at];
                energy += self.eq_buf[at].norm_sqr();
            }
            self.eq_head = (self.eq_head + 1) % self.eq_buf.len();

            // One symbol period delay
            let prev = self.delay_buf[self.head];
            self.delay_buf[self.head] = s;
            self.head = (self.head + 1) % self.sps;

            // Differential phase detector: d = s[n] * s[n - sps]^*
            let d = s * prev.conj();
            let d_phase = if d.norm_sqr() > 1e-12 { d.arg() } else { 0.0 };

            // Find the symbol instants: the sample phase where the matched
            // filter's output power peaks, averaged over ~30 symbols.
            let phase = self.sample_count % self.sps;
            let p = &mut self.phase_power[phase];
            *p += 0.03 * (filtered.norm_sqr() - *p);
            let strobe = self
                .phase_power
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .map(|(i, _)| i)
                == Some(phase);

            // Rotate the current equalizer output toward the nearest valid
            // differential phase, then use that complex error as the NLMS
            // teaching signal. Normalization bounds adaptation through fades.
            if self.eq_mu > 0.0 && strobe && prev.norm_sqr() > 1e-6 && s.norm_sqr() > 1e-6 {
                let raw = phase_error(d_phase);
                self.err_bias += 0.02 * (raw - self.err_bias);
                let err_phase = raw - self.err_bias;
                // Teach only on confident decisions: below ~12 dB a wrong
                // decision is common, and adapting toward it ran the taps
                // away (SER 0.15 -> 0.55 at 6 dB without this gate).
                let confident = err_phase.abs() < CONFIDENT_RAD;
                let (sin_neg, cos_neg) = (-err_phase).sin_cos();
                let correction = Complex32::new(cos_neg, sin_neg);
                let error = s * correction - s;
                let step = if confident { self.eq_mu / energy } else { 0.0 };
                for tap in 0..self.eq_taps.len() {
                    let at = (self.eq_head + self.eq_buf.len() - 1 - tap) % self.eq_buf.len();
                    self.eq_taps[tap] += error * self.eq_buf[at].conj() * step;
                }
            }
            self.sample_count = self.sample_count.wrapping_add(1);

            // Frequency estimate in Hz matching C4FM levels (±600, ±1800 Hz)
            let hz = d_phase * self.hz_per_rad;
            out.push(hz);
        }
    }

    pub fn reset(&mut self) {
        self.mf_buf.fill(Complex32::new(0.0, 0.0));
        self.mf_head = 0;
        self.delay_buf.fill(Complex32::new(0.0, 0.0));
        self.head = 0;
        self.eq_buf.fill(Complex32::new(0.0, 0.0));
        self.eq_head = 0;
        self.phase_power.fill(0.0);
        self.err_bias = 0.0;
        self.eq_taps[0] = Complex32::new(1.0, 0.0);
        self.phase_power.fill(0.0);
        self.sample_count = 0;
    }
}

/// Decision error (radians) under which the equalizer adapts; the
/// decision boundaries are π/4 away.
const CONFIDENT_RAD: f32 = 0.3;

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

    fn lcg(seed: &mut u64) -> f32 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((*seed >> 33) as f32 / (1u64 << 31) as f32) * 2.0 - 1.0
    }

    fn gauss(seed: &mut u64) -> f32 {
        (0..12).map(|_| lcg(seed)).sum::<f32>() / 2.0
    }

    /// π/4-DQPSK at 4800 Bd, RRC-shaped (α = 0.2) the way an LSM
    /// transmitter shapes it, at 48 kHz with a carrier offset, optional
    /// echo and AWGN at `esn0_db`. Returns the IQ and each symbol's dibit.
    fn lsm(symbols: usize, offset_hz: f32, echo: Option<(usize, Complex32)>, esn0_db: f32, seed: u64) -> (Vec<Complex32>, Vec<u8>) {
        let sps = 10usize;
        let taps = crate::p25::rrc_taps(sps, 8, 0.2);
        let mut state = seed;
        let mut phase = 0.0f32;
        let mut dibits = Vec::with_capacity(symbols);
        let mut impulses = vec![Complex32::new(0.0, 0.0); symbols * sps + taps.len()];
        for k in 0..symbols {
            let d = (((lcg(&mut state) + 1.0) * 2.0) as u8).min(3);
            dibits.push(d);
            phase += match d {
                1 => 3.0 * std::f32::consts::FRAC_PI_4,
                0 => std::f32::consts::FRAC_PI_4,
                2 => -std::f32::consts::FRAC_PI_4,
                _ => -3.0 * std::f32::consts::FRAC_PI_4,
            };
            impulses[k * sps] = Complex32::new(phase.cos(), phase.sin());
        }
        let mut clean: Vec<Complex32> = (0..impulses.len())
            .map(|n| {
                let mut acc = Complex32::new(0.0, 0.0);
                for (j, &t) in taps.iter().enumerate() {
                    if n >= j {
                        acc += impulses[n - j] * t;
                    }
                }
                acc
            })
            .collect();
        if let Some((delay, gain)) = echo {
            let dry = clean.clone();
            for n in delay..clean.len() {
                clean[n] += dry[n - delay] * gain;
            }
        }
        // Es: energy per symbol, the sum over one symbol period.
        let es: f32 = clean.iter().map(|x| x.norm_sqr()).sum::<f32>() / symbols as f32;
        let n0 = es / 10f32.powf(esn0_db / 10.0);
        let sigma = (n0 / 2.0).sqrt();
        let iq = clean
            .iter()
            .enumerate()
            .map(|(n, &x)| {
                let rot = TAU * offset_hz * n as f32 / 48_000.0;
                x * Complex32::new(rot.cos(), rot.sin()) + Complex32::new(gauss(&mut state), gauss(&mut state)) * sigma
            })
            .collect();
        (iq, dibits)
    }

    /// The production configuration, as the P25 conventional receiver
    /// builds it.
    fn production() -> CqpskDemodulator {
        CqpskDemodulator::new(48_000.0, 4800.0).with_equalizer(7, 0.02)
    }

    /// Symbol error rate of `demod` over `lsm`, at the best sample phase
    /// and lag (the shaping filters delay the symbols by several periods) —
    /// what the frame detector's sync alignment picks.
    fn ser(mut demod: CqpskDemodulator, iq: &[Complex32], dibits: &[u8]) -> f32 {
        let mut hz = Vec::new();
        demod.process(iq, &mut hz);
        let skip = 300;
        (0..120)
            .map(|lag| {
                let mut errors = 0;
                let mut total = 0;
                for k in skip..dibits.len() - 20 {
                    let Some(&h) = hz.get(k * 10 + lag) else { break };
                    total += 1;
                    if crate::p25::slice_dibit(h, 1800.0) != dibits[k] {
                        errors += 1;
                    }
                }
                errors as f32 / total.max(1) as f32
            })
            .fold(1.0, f32::min)
    }

    /// Measurement sweep, not a pass/fail check: `cargo test -p
    /// scannerd-engine --lib cqpsk_sensitivity_sweep -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn cqpsk_sensitivity_sweep() {
        let echoes = [
            ("no echo", None),
            ("echo 3 smp", Some((3, Complex32::new(0.4, 0.25)))),
            ("echo 1 sym", Some((10, Complex32::new(0.3, -0.3)))),
        ];
        for (label, echo) in echoes {
            for offset in [0.0, 60.0] {
                for esn0 in [6.0, 8.0, 10.0, 12.0, 14.0, 16.0, 20.0, 30.0] {
                    let (iq, dibits) = lsm(6000, offset, echo, esn0, 11);
                    eprintln!(
                        "{label:>10} offset {offset:>3.0} Hz Es/N0 {esn0:>4.1} dB: SER production {:.4}  no equalizer {:.4}",
                        ser(production(), &iq, &dibits),
                        ser(CqpskDemodulator::new(48_000.0, 4800.0), &iq, &dibits),
                    );
                }
            }
        }
    }

    /// A clean LSM signal comes back symbol for symbol, at the C4FM
    /// levels the frame detector slices against.
    #[test]
    fn clean_lsm_demodulates_to_the_c4fm_levels() {
        let (iq, dibits) = lsm(600, 0.0, None, 60.0, 3);
        assert_eq!(ser(production(), &iq, &dibits), 0.0);
        let mut hz = Vec::new();
        CqpskDemodulator::new(48_000.0, 4800.0).process(&iq, &mut hz);
        // At the symbol instants (the lag whose samples slice back to the
        // sent dibits), the outer symbols sit at ±1800 Hz.
        let lag = (0..120)
            .find(|&lag| {
                (300..500).all(|k| crate::p25::slice_dibit(hz[k * 10 + lag], 1800.0) == dibits[k])
            })
            .expect("a lag that reads the symbols");
        let outer: Vec<f32> =
            (300..500).filter(|&k| dibits[k] & 1 == 1).map(|k| hz[k * 10 + lag].abs()).collect();
        let mean = outer.iter().sum::<f32>() / outer.len() as f32;
        assert!((mean - 1800.0).abs() < 60.0, "outer level {mean:.0} Hz");
    }

    /// The matched filter is where the sensitivity is: at 10 dB Es/N0 the
    /// old filterless chain with its per-sample carrier loop missed 74 % of
    /// symbols (and 73 % even at 20 dB); now under 3 %, with a 60 Hz offset
    /// and a 3-sample echo too.
    #[test]
    fn production_chain_reads_lsm_at_10_db() {
        for (offset, echo) in [(0.0, None), (60.0, Some((3, Complex32::new(0.4, 0.25))))] {
            let (iq, dibits) = lsm(3000, offset, echo, 10.0, 11);
            let rate = ser(production(), &iq, &dibits);
            assert!(rate < 0.04, "offset {offset} echo {echo:?}: SER {rate:.4}");
        }
    }

    /// A one-symbol simulcast echo is what the equalizer is for: at 30 dB
    /// it takes the symbol error rate from ~6 % to under 1 %.
    #[test]
    fn the_equalizer_cancels_a_one_symbol_echo() {
        let (iq, dibits) = lsm(6000, 0.0, Some((10, Complex32::new(0.3, -0.3))), 30.0, 11);
        let plain = ser(CqpskDemodulator::new(48_000.0, 4800.0), &iq, &dibits);
        let equalized = ser(production(), &iq, &dibits);
        assert!(equalized < plain * 0.3, "equalized {equalized:.4} vs plain {plain:.4}");
    }
}
