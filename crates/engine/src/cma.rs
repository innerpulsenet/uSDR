//! A blind equalizer for broadcast FM: undoes multipath before the
//! discriminator.
//!
//! An FM signal's envelope is constant; whatever ripples it on the way in
//! is the channel. Reflections off buildings and hills arrive a few
//! microseconds behind the direct signal and beat with it, and the beat
//! follows the program: the discriminator turns it into distortion across
//! the whole multiplex, worst in the stereo difference at 23–53 kHz, which
//! is why a strong station with an indoor antenna can sound grainy and
//! blend toward mono while measuring forty or fifty decibels over the noise.
//!
//! The constant-modulus algorithm (Godard; Treichler and Agee) adapts a
//! short complex FIR to drive the output's envelope back to flat, which for
//! FM is the same as undoing the channel. It needs no training and nothing
//! from the station but the constancy FM already promises. The input is
//! normalized by its own slowly tracked power, so the step size means the
//! same thing on every station, and a little leakage keeps the taps from
//! wandering on a channel that is already clean.

use num_complex::Complex32;

/// Taps, at the 256 kHz channel rate: 3.9 µs each, so 32 span 125 µs, past
/// the delays of most reflections a fixed antenna hears. Sixteen undid one
/// echo but not two: inverting a channel takes several times its delay. The
/// reference tap sits a quarter in, leaving room for echoes ahead of the
/// strongest path.
pub const TAPS: usize = 32;
const REF_TAP: usize = 8;
/// Adaptation step, per tap, for unit input power. The channel changes as
/// traffic and aircraft move; this follows it in tens of milliseconds.
const MU: f32 = 0.001;
/// The equalizer is in the path only while the channel ripples the
/// envelope this much and the output is clearly flatter for it: a clean
/// channel has nothing to undo, and the taps' own settling would cost it a
/// few decibels.
const ENGAGE_RIPPLE: f32 = 0.03;
const RELEASE_RIPPLE: f32 = 0.02;
/// Ripple is tracked over about a quarter second, judged every 1024
/// samples, and a change of state holds at least two seconds; the switch
/// itself is a 1 ms crossfade between paths of the same delay.
const RIPPLE_SAMPLES: f32 = 65_536.0;
const JUDGE_EVERY: u32 = 1_024;
const HOLD_SAMPLES: u32 = 512_000;
const FADE_SAMPLES: f32 = 256.0;
/// Leakage per update, toward the reference tap alone.
const LEAK: f32 = 1e-5;
/// The input power is tracked over about this many samples, except that a
/// sample this many times the tracked power resets it.
const POWER_SAMPLES: f32 = 4_096.0;
const SNAP_RISE: f32 = 4.0;
/// Output power, against the input's tracked power of one, past which the
/// taps have run away: a channel inverse keeps it near one.
const RUNAWAY_POWER: f32 = 100.0;

pub struct Cma {
    mu: f32,
    taps: [Complex32; TAPS],
    /// Twice the taps long, written at both halves, so the newest `TAPS`
    /// samples are always one contiguous slice.
    hist: [Complex32; 2 * TAPS],
    at: usize,
    power: f32,
    /// The envelope's spread before and after, accumulated for `ripple`.
    stats: [f64; 5],
    /// The gate: the envelope's mean and square in and out, tracked; whether
    /// the equalized path is wanted, how far the crossfade is toward it,
    /// and the samples to the next judgment and since the last change.
    env: [f32; 4],
    engaged: bool,
    mix: f32,
    until_judge: u32,
    since_change: u32,
}

impl Default for Cma {
    fn default() -> Self {
        Self::new()
    }
}

impl Cma {
    pub fn new() -> Self {
        Self::with_step(MU)
    }

    pub fn with_step(mu: f32) -> Self {
        let mut taps = [Complex32::new(0.0, 0.0); TAPS];
        taps[REF_TAP] = Complex32::new(1.0, 0.0);
        Cma {
            mu,
            taps,
            hist: [Complex32::new(0.0, 0.0); 2 * TAPS],
            at: 0,
            power: 0.0,
            stats: [0.0; 5],
            env: [1.0, 1.0, 1.0, 1.0],
            engaged: false,
            mix: 0.0,
            until_judge: JUDGE_EVERY,
            since_change: 0,
        }
    }

    /// The envelope's spread in and out over the last quarter second, as a
    /// fraction of its mean: how much multipath the channel has, and how
    /// much is left after the equalizer.
    pub fn ripple_now(&self) -> (f32, f32) {
        let spread = |m: f32, sq: f32| (sq - m * m).max(0.0).sqrt() / m.max(1e-12);
        (spread(self.env[0], self.env[1]), spread(self.env[2], self.env[3]))
    }

    /// The equalized path is in use (or fading in).
    pub fn engaged(&self) -> bool {
        self.engaged
    }

    pub fn reset(&mut self) {
        *self = Self::with_step(self.mu);
    }

    /// Equalize a block in place.
    pub fn process(&mut self, iq: &mut [Complex32]) {
        let k_pow = 1.0 / POWER_SAMPLES;
        for z in iq.iter_mut() {
            let x = *z;
            let p = x.norm_sqr();
            // A step up (a retune landing on a station, the tuner settling)
            // is taken at once: normalized by the quiet that came before,
            // the station would reach the taps tens of decibels hot, and the
            // update would throw them to infinity in a few samples.
            if self.power == 0.0 || p > SNAP_RISE * self.power {
                // The history was normalized by the old power: rescale it so
                // the plain path's delayed samples keep their level.
                let old = self.power.max(1e-20);
                self.power = p.max(1e-20);
                let r = (old / self.power).sqrt();
                for h in self.hist.iter_mut() {
                    *h *= r;
                }
            }
            self.power += (p - self.power) * k_pow;
            let g = 1.0 / self.power.max(1e-20).sqrt();
            let xn = x * g;
            // Newest first: hist[at..at + TAPS] runs newest to oldest.
            self.at = if self.at == 0 { TAPS - 1 } else { self.at - 1 };
            self.hist[self.at] = xn;
            self.hist[self.at + TAPS] = xn;
            let win = &self.hist[self.at..self.at + TAPS];
            let mut y = Complex32::new(0.0, 0.0);
            for (w, h) in self.taps.iter().zip(win) {
                y += w * h;
            }
            // Drive |y|² to one; the gradient of (|y|² − 1)² against each tap.
            let plain = win[REF_TAP];
            let mut m = y.norm_sqr();
            // Taps that ran away anyway (or NaN, which the gate would read
            // as a flat envelope and engage) start over on the plain path.
            if !(m < RUNAWAY_POWER) {
                // Field by field: `win` still borrows the history.
                self.taps = [Complex32::new(0.0, 0.0); TAPS];
                self.taps[REF_TAP] = Complex32::new(1.0, 0.0);
                (self.engaged, self.mix, self.since_change) = (false, 0.0, 0);
                y = plain;
                m = y.norm_sqr();
            }
            let e = y * (m - 1.0) * self.mu;
            for (w, h) in self.taps.iter_mut().zip(win) {
                *w -= e * h.conj();
            }
            // A little leakage toward the plain reference tap.
            for (i, w) in self.taps.iter_mut().enumerate() {
                let target = if i == REF_TAP { 1.0 } else { 0.0 };
                w.re += (target - w.re) * LEAK;
                w.im -= w.im * LEAK;
            }
            let a_in = (p * g * g).sqrt();
            let a_out = m.sqrt();
            self.stats[0] += a_in as f64;
            self.stats[1] += (a_in * a_in) as f64;
            self.stats[2] += a_out as f64;
            self.stats[3] += (a_out * a_out) as f64;
            self.stats[4] += 1.0;
            let k = 1.0 / RIPPLE_SAMPLES;
            for (e, v) in self.env.iter_mut().zip([a_in, a_in * a_in, a_out, a_out * a_out]) {
                *e += (v - *e) * k;
            }
            self.since_change = self.since_change.saturating_add(1);
            self.until_judge -= 1;
            if self.until_judge == 0 {
                self.until_judge = JUDGE_EVERY;
                self.judge();
            }
            let target = if self.engaged { 1.0 } else { 0.0 };
            self.mix += (target - self.mix).clamp(-1.0 / FADE_SAMPLES, 1.0 / FADE_SAMPLES);
            // The plain path is the input delayed to the reference tap, so
            // the two line up and the crossfade between them is seamless.
            let out = if self.mix <= 0.0 { plain } else if self.mix >= 1.0 { y } else { y * self.mix + plain * (1.0 - self.mix) };
            // The output carries the input's level back, so nothing after
            // this sees a change of scale.
            *z = out / g;
        }
    }

    fn judge(&mut self) {
        let spread = |m: f32, sq: f32| (sq - m * m).max(0.0).sqrt() / m.max(1e-12);
        let (r_in, r_out) = (spread(self.env[0], self.env[1]), spread(self.env[2], self.env[3]));
        if self.since_change < HOLD_SAMPLES {
            return;
        }
        let want = if self.engaged {
            !(r_in < RELEASE_RIPPLE || r_out > 0.95 * r_in)
        } else {
            r_out.is_finite() && r_in > ENGAGE_RIPPLE && r_out < 0.8 * r_in
        };
        if want != self.engaged {
            self.engaged = want;
            self.since_change = 0;
        }
    }

    /// The envelope's spread, as a fraction of its mean, before and after
    /// the equalizer since the last call: what the channel did, and what is
    /// left of it.
    pub fn ripple(&mut self) -> Option<(f64, f64)> {
        let s = std::mem::take(&mut self.stats);
        if s[4] < 1.0 {
            return None;
        }
        let spread = |sum: f64, sq: f64| {
            let mean = sum / s[4];
            (sq / s[4] - mean * mean).max(0.0).sqrt() / mean.max(1e-12)
        };
        Some((spread(s[0], s[1]), spread(s[2], s[3])))
    }

    /// The strongest tap's share of the taps' energy: one for a channel
    /// with nothing to undo, less as the equalizer takes echoes out.
    pub fn main_tap_share(&self) -> f32 {
        let total: f32 = self.taps.iter().map(|w| w.norm_sqr()).sum();
        let best = self.taps.iter().map(|w| w.norm_sqr()).fold(0.0, f32::max);
        best / total.max(1e-20)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nbfm::NbfmDemod;
    use crate::stereo::WfmAudio;
    use std::f64::consts::TAU;

    const FS: f64 = 256_000.0;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
        fn gauss(&mut self) -> f64 {
            let u = self.next().max(1e-300);
            (-2.0 * u.ln()).sqrt() * (TAU * self.next()).cos()
        }
    }

    /// Loud wide stereo program on a station, as I/Q at the channel rate,
    /// through a channel of (delay in samples, gain, phase) paths, with
    /// noise `snr_db` under the carrier.
    fn station(secs: f64, paths: &[(usize, f64, f64)], snr_db: f64, seed: u64) -> Vec<Complex32> {
        let n = (secs * FS) as usize;
        let mut ph = 0.0f64;
        let tx: Vec<(f64, f64)> = (0..n)
            .map(|k| {
                let t = k as f64 / FS;
                let w = TAU * 19_000.0 * t;
                let l = 0.6 * (TAU * 400.0 * t).sin() + 0.3 * (TAU * 5_100.0 * t).sin();
                let r = 0.6 * (TAU * 1_300.0 * t).sin() + 0.3 * (TAU * 9_700.0 * t).sin();
                let mpx = 0.9 * ((l + r) / 2.0 + (l - r) / 2.0 * (2.0 * w).sin()) + 0.1 * w.sin();
                ph += TAU * 75_000.0 * mpx / FS;
                (ph.cos(), ph.sin())
            })
            .collect();
        let mut rng = Rng(seed);
        let sigma = (10f64.powf(-snr_db / 10.0) / 2.0).sqrt();
        (0..n)
            .map(|k| {
                let (mut re, mut im) = (0.0, 0.0);
                for &(d, g, p) in paths {
                    if k >= d {
                        let (a, b) = tx[k - d];
                        let (c, s) = (p.cos() * g, p.sin() * g);
                        re += a * c - b * s;
                        im += a * s + b * c;
                    }
                }
                Complex32::new((re + sigma * rng.gauss()) as f32, (im + sigma * rng.gauss()) as f32)
            })
            .collect()
    }

    /// The stereo decoder's S/N reading after the receiver's discriminator,
    /// averaged (as noise power) over the last second: one reading depends
    /// on where in the program it falls.
    fn stereo_snr(iq: &[Complex32]) -> f32 {
        let mut fm = NbfmDemod::with_deviation(FS, 75_000.0);
        let mut dec = WfmAudio::new(FS, 5, 75_000.0, 75e-6);
        let (mut disc, mut mono, mut st) = (Vec::new(), Vec::new(), Vec::new());
        let from = iq.len().saturating_sub(FS as usize);
        let (mut noise, mut n) = (0.0f64, 0);
        for (i, c) in iq.chunks(1024).enumerate() {
            disc.clear();
            fm.discriminator_hz(c, &mut disc);
            dec.process(&disc, &mut mono, &mut st);
            if i * 1024 >= from
                && let Some(db) = dec.status().snr_db
            {
                noise += 10f64.powf(-db as f64 / 10.0);
                n += 1;
            }
        }
        if n == 0 { 0.0 } else { (-10.0 * (noise / n as f64).log10()) as f32 }
    }

    /// The input delayed as the equalizer delays it, for comparing like
    /// with like.
    fn delayed(iq: &[Complex32]) -> Vec<Complex32> {
        std::iter::repeat_n(iq[0], REF_TAP).chain(iq.iter().copied()).take(iq.len()).collect()
    }

    fn equalized(iq: &[Complex32]) -> (Vec<Complex32>, Cma) {
        let mut eq = Cma::new();
        let mut out = iq.to_vec();
        for c in out.chunks_mut(4096) {
            eq.process(c);
        }
        (out, eq)
    }

    #[test]
    fn multipath_is_undone() {
        // A reflection 0.5 of the direct path, 11.7 µs behind, and a weaker
        // one 23 µs behind: the kind of channel an indoor antenna sees.
        let paths = [(0, 1.0, 0.0), (3, 0.5, 2.1), (6, 0.25, -0.7)];
        let iq = station(4.0, &paths, 45.0, 1);
        let before = stereo_snr(&delayed(&iq));
        let (out, mut eq) = equalized(&iq);
        let (r_in, _) = eq.ripple().unwrap();
        // The last second only, once it has converged.
        let tail = &out[out.len() - FS as usize..];
        let mut eq2 = Cma::new();
        let mut t2 = iq[iq.len() - FS as usize..].to_vec();
        eq2.process(&mut t2);
        let after = stereo_snr(&out);
        let ripple_after = {
            let a: Vec<f64> = tail.iter().map(|z| z.norm() as f64).collect();
            let m = a.iter().sum::<f64>() / a.len() as f64;
            (a.iter().map(|v| (v - m).powi(2)).sum::<f64>() / a.len() as f64).sqrt() / m
        };
        println!("ripple {:.1}% → {:.1}%, stereo S/N {before:.1} → {after:.1} dB, main tap {:.2}", 100.0 * r_in, 100.0 * ripple_after, eq.main_tap_share());
        assert!(r_in > 0.2, "the channel should ripple the envelope: {r_in}");
        assert!(ripple_after < 0.05, "ripple left {ripple_after}");
        assert!(after > before + 10.0, "stereo S/N {before:.1} → {after:.1}");
        assert!(eq.engaged());
    }

    #[test]
    fn a_clean_channel_is_left_alone() {
        let iq = station(3.0, &[(0, 1.0, 0.0)], 45.0, 2);
        let before = stereo_snr(&delayed(&iq));
        let (out, eq) = equalized(&iq);
        let after = stereo_snr(&out);
        println!("stereo S/N {before:.1} → {after:.1} dB, main tap {:.3}", eq.main_tap_share());
        assert!((after - before).abs() < 0.5, "{before:.1} → {after:.1}");
        assert!(!eq.engaged(), "nothing to undo, so the plain path");
        assert!(eq.main_tap_share() > 0.95);
    }

    #[test]
    fn a_station_after_quiet() {
        // A retune: the channel hears near nothing (the tuner between bands,
        // the stream reopening) and then the station, 40 to 80 dB up. The
        // taps once ran to NaN on that step, the gate took NaN for a flat
        // envelope and engaged, and the pilot and RDS went for good.
        let paths = [(0, 1.0, 0.0), (2, 0.1, 1.0)];
        let st = station(4.0, &paths, 45.0, 7);
        let (direct, _) = equalized(&st);
        let want = stereo_snr(&direct[direct.len() - FS as usize..]);
        let mut rng = Rng(9);
        for (quiet_secs, level) in [(0.02, 1e-4f32), (0.05, 0.0), (0.2, 1e-3), (0.3, 1e-2)] {
            let mut iq: Vec<Complex32> = (0..(quiet_secs * FS) as usize)
                .map(|_| Complex32::new(rng.gauss() as f32, rng.gauss() as f32) * level)
                .collect();
            iq.extend(st.iter().map(|z| z * 0.05));
            let (out, eq) = equalized(&iq);
            let got = stereo_snr(&out[out.len() - FS as usize..]);
            println!("{quiet_secs} s at {level}: stereo S/N {got:.1} dB (no step {want:.1}), main tap {:.3}", eq.main_tap_share());
            assert!(out.iter().all(|z| z.re.is_finite() && z.im.is_finite()));
            assert!(eq.main_tap_share() > 0.9, "{}", eq.main_tap_share());
            assert!(got > want - 1.0, "{got:.1} against {want:.1}");
        }
    }

    #[test]
    fn the_level_is_kept() {
        let iq: Vec<Complex32> = station(1.0, &[(0, 1.0, 0.0)], 40.0, 3).into_iter().map(|z| z * 0.003).collect();
        let (out, _) = equalized(&iq);
        let p = |v: &[Complex32]| v.iter().map(|z| z.norm_sqr() as f64).sum::<f64>() / v.len() as f64;
        let ratio = p(&out[out.len() / 2..]) / p(&iq[iq.len() / 2..]);
        assert!((ratio - 1.0).abs() < 0.1, "{ratio}");
    }

    #[test]
    #[ignore]
    fn step_sweep() {
        let multi = station(5.0, &[(0, 1.0, 0.0), (3, 0.5, 2.1), (6, 0.25, -0.7)], 45.0, 1);
        let mild = station(5.0, &[(0, 1.0, 0.0), (2, 0.2, 1.0)], 45.0, 5);
        let clean = station(5.0, &[(0, 1.0, 0.0)], 45.0, 2);
        let (m0, d0, c0) = (stereo_snr(&delayed(&multi)), stereo_snr(&delayed(&mild)), stereo_snr(&delayed(&clean)));
        println!("unequalized: multipath {m0:.1}  mild {d0:.1}  clean {c0:.1}");
        for mu in [0.002f32, 0.001, 0.0005, 0.00025, 0.000125, 0.0000625] {
            let run = |iq: &[Complex32]| {
                let mut eq = Cma::with_step(mu);
                let mut out = iq.to_vec();
                for c in out.chunks_mut(4096) {
                    eq.process(c);
                }
                stereo_snr(&out)
            };
            println!("mu {mu:.7}: multipath {:.1}  mild {:.1}  clean {:.1}", run(&multi), run(&mild), run(&clean));
        }
    }

    #[test]
    fn it_is_cheap() {
        let iq = station(5.0, &[(0, 1.0, 0.0), (3, 0.5, 2.1)], 40.0, 4);
        let mut eq = Cma::new();
        let mut buf = iq.clone();
        let t = std::time::Instant::now();
        for c in buf.chunks_mut(4096) {
            eq.process(c);
        }
        let secs = t.elapsed().as_secs_f64();
        println!("{:.0}x realtime, {:.2}% of a core", 5.0 / secs, 100.0 * secs / 5.0);
    }
}
