//! A fractional resampler: complex samples at any rate to any other, by a
//! Kaiser-windowed sinc evaluated between tabulated phases.
//!
//! The DRM receiver runs at exactly 48 kHz so that every OFDM length is a
//! whole number of samples; the radio delivers something near it (47 904 Hz
//! live). The ratio is fixed: the transmitter's own clock error is taken up
//! by the receiver's timing loop, not here.

use num_complex::Complex32;

pub struct Resampler {
    /// Input samples per output sample.
    step: f64,
    taps: usize,
    phases: usize,
    /// (phases + 1) rows of `taps` coefficients.
    table: Vec<f32>,
    hist: Vec<Complex32>,
    /// Position of the next output, in input samples from `hist[0]`.
    t: f64,
}

fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    for k in 1..40 {
        term *= (x / (2.0 * k as f64)).powi(2);
        sum += term;
    }
    sum
}

impl Resampler {
    /// From `fs_in` to `fs_out`, with a filter of `taps` (even, rounded
    /// down) cut off halfway into the band the lower rate leaves free.
    pub fn new(fs_in: f64, fs_out: f64, taps: usize) -> Self {
        let taps = taps.max(4) & !1;
        let phases = 512;
        let step = fs_in / fs_out;
        // Cut-off in cycles per input sample: halfway into the band the
        // lower of the two rates leaves free.
        let fc = 0.5 * (fs_out / fs_in).min(1.0);
        let beta = 7.0;
        let half = taps as f64 / 2.0;
        let mut table = vec![0f32; (phases + 1) * taps];
        for p in 0..=phases {
            let frac = p as f64 / phases as f64;
            let row = &mut table[p * taps..(p + 1) * taps];
            let mut sum = 0.0;
            for (j, c) in row.iter_mut().enumerate() {
                // Tap j multiplies input floor(t) - half + 1 + j; its
                // distance from t is frac + half - 1 - j.
                let d = frac + half - 1.0 - j as f64;
                let x = 2.0 * fc * d;
                let sinc =
                    if x.abs() < 1e-12 { 1.0 } else { (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x) };
                let r = d / half;
                let w = if r.abs() >= 1.0 { 0.0 } else { bessel_i0(beta * (1.0 - r * r).sqrt()) / bessel_i0(beta) };
                let v = 2.0 * fc * sinc * w;
                *c = v as f32;
                sum += v;
            }
            for c in row.iter_mut() {
                *c = (*c as f64 / sum) as f32;
            }
        }
        Resampler { step, taps, phases, table, hist: vec![Complex32::new(0.0, 0.0); taps], t: (taps / 2 - 1) as f64 }
    }

    pub fn process(&mut self, input: &[Complex32], out: &mut Vec<Complex32>) {
        self.hist.extend_from_slice(input);
        let half = self.taps / 2;
        loop {
            let base = self.t.floor() as usize;
            if base + half >= self.hist.len() {
                break;
            }
            let frac = self.t - base as f64;
            let pf = frac * self.phases as f64;
            let p = (pf as usize).min(self.phases - 1);
            let mu = (pf - p as f64) as f32;
            let a = &self.table[p * self.taps..(p + 1) * self.taps];
            let b = &self.table[(p + 1) * self.taps..(p + 2) * self.taps];
            let x = &self.hist[base + 1 - half..base + 1 + half];
            let mut acc = Complex32::new(0.0, 0.0);
            for j in 0..self.taps {
                acc += x[j] * (a[j] + mu * (b[j] - a[j]));
            }
            out.push(acc);
            self.t += self.step;
        }
        // Drop what no later output needs.
        let keep_from = (self.t.floor() as usize + 1).saturating_sub(half);
        if keep_from > 0 {
            self.hist.drain(..keep_from);
            self.t -= keep_from as f64;
        }
    }
}
