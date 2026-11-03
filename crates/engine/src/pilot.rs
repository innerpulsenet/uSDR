//! The 19 kHz stereo pilot, which broadcast FM's 38 kHz stereo subcarrier
//! and 57 kHz RDS subcarrier are both locked to: one PLL shape shared by the
//! RDS decoder (three times its phase) and the stereo decoder (twice it).

use std::f64::consts::TAU;

pub(crate) const PILOT_HZ: f64 = 19_000.0;

/// A 19 kHz PLL updated once per short block, with a lock detector.
#[derive(Clone, Debug)]
pub(crate) struct PilotPll {
    /// Current phasor (cos, sin) and its per-sample step.
    c: f64,
    s: f64,
    step_c: f64,
    step_s: f64,
    /// Radians per sample.
    freq: f64,
    center: f64,
    pub(crate) fs: f64,
    /// Correlation accumulated over the current block.
    acc_i: f64,
    acc_q: f64,
    n: usize,
    block: usize,
    kp: f64,
    ki: f64,
    /// Smoothed cos(phase error): 1 locked, ~0 on noise.
    lock: f64,
    pub(crate) locked: bool,
}

impl PilotPll {
    pub(crate) fn new(fs: f64) -> Self {
        // A 250 µs block: four and three-quarter pilot cycles, nine and a half
        // of the 38 kHz product, which the sum all but cancels.
        let block = (fs / 4_000.0).round().max(4.0) as usize;
        let t = block as f64 / fs;
        let wn = TAU * 8.0 * t;
        let center = TAU * PILOT_HZ / fs;
        let mut p = Self {
            c: 1.0,
            s: 0.0,
            step_c: 1.0,
            step_s: 0.0,
            freq: center,
            center,
            fs,
            acc_i: 0.0,
            acc_q: 0.0,
            n: 0,
            block,
            kp: 2.0 * 0.707 * wn,
            ki: wn * wn,
            lock: 0.0,
            locked: false,
        };
        p.set_step();
        p
    }

    fn set_step(&mut self) {
        let (s, c) = self.freq.sin_cos();
        self.step_c = c;
        self.step_s = s;
    }

    /// Advance one sample with input `x`; returns the pilot phasor in use for
    /// this sample.
    #[inline]
    pub(crate) fn step(&mut self, x: f64) -> (f64, f64) {
        let (c, s) = (self.c, self.s);
        self.acc_i += x * c;
        self.acc_q -= x * s;
        let nc = c * self.step_c - s * self.step_s;
        let ns = s * self.step_c + c * self.step_s;
        self.c = nc;
        self.s = ns;
        self.n += 1;
        if self.n >= self.block {
            self.update();
        }
        (c, s)
    }

    fn update(&mut self) {
        let mag = self.acc_i.hypot(self.acc_q);
        if mag > 0.0 {
            let err = self.acc_q.atan2(self.acc_i);
            // The loop's own integrator, held near 19 kHz so that following
            // noise cannot walk it somewhere a real pilot would not be.
            let max = TAU * 20.0 / self.fs;
            let k = self.block as f64;
            self.freq = (self.freq + self.ki * err / k).clamp(self.center - max, self.center + max);
            // Phase correction applied by rotating the phasor.
            let (ps, pc) = (self.kp * err).sin_cos();
            let (c, s) = (self.c, self.s);
            self.c = c * pc - s * ps;
            self.s = s * pc + c * ps;
            // Smoothed over about a quarter second.
            self.lock += (self.acc_i / mag - self.lock) * 0.004;
            if self.locked && self.lock < 0.5 {
                self.locked = false;
            } else if !self.locked && self.lock > 0.8 {
                self.locked = true;
            }
        }
        // Keep the phasor on the unit circle.
        let r = self.c.hypot(self.s);
        if r > 0.0 {
            self.c /= r;
            self.s /= r;
        }
        self.set_step();
        self.acc_i = 0.0;
        self.acc_q = 0.0;
        self.n = 0;
    }
}
