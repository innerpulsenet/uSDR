//! Test transmitter back end and HF channel: PSK symbols shaped by a
//! root-raised-cosine pulse onto a carrier at audio, with a carrier offset,
//! a sample-clock error, multipath with Watterson fading (CCIR 549: each
//! path Rayleigh with a Gaussian Doppler spectrum), and white noise set
//! against the signal's power in 3 kHz.

use num_complex::{Complex32, Complex64};
use std::f64::consts::PI;

/// xorshift64*, for runs that are the same every time.
pub(super) struct Rng(pub(super) u64);

impl Rng {
    pub(super) fn uniform(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64 / (1u64 << 53) as f64
    }

    pub(super) fn gauss(&mut self) -> f64 {
        let u = self.uniform().max(1e-300);
        let v = self.uniform();
        (-2.0 * u.ln()).sqrt() * (2.0 * PI * v).cos()
    }

    pub(super) fn byte(&mut self) -> u8 {
        (self.uniform() * 256.0) as u8
    }
}

#[derive(Clone, Copy)]
pub(super) struct Path {
    pub(super) delay_s: f64,
    /// Mean power share.
    pub(super) power: f64,
}

#[derive(Clone)]
pub(super) struct Channel {
    pub(super) fs: f64,
    pub(super) carrier_hz: f64,
    pub(super) offset_hz: f64,
    /// Receiver sample clock error: the signal's time runs this many parts
    /// per million fast.
    pub(super) ppm: f64,
    pub(super) paths: Vec<Path>,
    /// Two-sigma Doppler spread of every path; none for fixed paths.
    pub(super) spread_hz: Option<f64>,
    /// Signal to noise in 3 kHz; none for no noise.
    pub(super) snr_db: Option<f64>,
    pub(super) lead_s: f64,
    pub(super) tail_s: f64,
    pub(super) seed: u64,
}

impl Channel {
    pub(super) fn clean(fs: f64) -> Self {
        Self {
            fs,
            carrier_hz: 1800.0,
            offset_hz: 0.0,
            ppm: 0.0,
            paths: vec![Path { delay_s: 0.0, power: 1.0 }],
            spread_hz: None,
            snr_db: None,
            lead_s: 0.5,
            tail_s: 0.5,
            seed: 1,
        }
    }

    /// The CCIR poor channel: two equal paths 2 ms apart, 1 Hz spread.
    pub(super) fn poor(fs: f64, snr_db: f64) -> Self {
        Self {
            paths: vec![Path { delay_s: 0.0, power: 0.5 }, Path { delay_s: 2e-3, power: 0.5 }],
            spread_hz: Some(1.0),
            snr_db: Some(snr_db),
            ..Self::clean(fs)
        }
    }

    /// CCIR moderate: two equal paths 1 ms apart, 0.5 Hz spread.
    pub(super) fn moderate(fs: f64, snr_db: f64) -> Self {
        Self {
            paths: vec![Path { delay_s: 0.0, power: 0.5 }, Path { delay_s: 1e-3, power: 0.5 }],
            spread_hz: Some(0.5),
            snr_db: Some(snr_db),
            ..Self::clean(fs)
        }
    }
}

/// Root raised cosine, roll-off `a`, at `t` symbols from its center.
fn rrc(t: f64, a: f64) -> f64 {
    if t.abs() < 1e-9 {
        return 1.0 - a + 4.0 * a / PI;
    }
    if (t.abs() - 1.0 / (4.0 * a)).abs() < 1e-9 {
        return a / 2f64.sqrt() * ((1.0 + 2.0 / PI) * (PI / (4.0 * a)).sin() + (1.0 - 2.0 / PI) * (PI / (4.0 * a)).cos());
    }
    let num = (PI * t * (1.0 - a)).sin() + 4.0 * a * t * (PI * t * (1.0 + a)).cos();
    let den = PI * t * (1.0 - (4.0 * a * t).powi(2));
    num / den
}

/// One path's complex gain at `rate` Hz for `n` samples, mean power one:
/// white noise shaped by a Gaussian Doppler spectrum.
fn fading(n: usize, rate: f64, spread_hz: f64, seed: u64) -> Vec<Complex64> {
    let len = n.next_power_of_two().max(1024);
    let mut rng = Rng(seed | 1);
    let sigma = spread_hz / 2.0;
    let mut x: Vec<Complex64> = (0..len)
        .map(|k| {
            let f = if k <= len / 2 { k as f64 } else { k as f64 - len as f64 } * rate / len as f64;
            let g = (-f * f / (4.0 * sigma * sigma)).exp();
            Complex64::new(rng.gauss() * g, rng.gauss() * g)
        })
        .collect();
    rustfft::FftPlanner::new().plan_fft_inverse(len).process(&mut x);
    let p = x.iter().map(|v| v.norm_sqr()).sum::<f64>() / len as f64;
    let k = (1.0 / p).sqrt();
    x.truncate(n);
    x.iter_mut().for_each(|v| *v *= k);
    x
}

const SPAN: i64 = 8;
const TABLE: usize = 256;
const ROLLOFF: f64 = 0.25;

/// `syms` at `baud` through the channel, as real audio.
pub(super) fn modulate(syms: &[Complex32], baud: f64, ch: &Channel) -> Vec<f32> {
    let table: Vec<f64> = (0..=(2 * SPAN as usize * TABLE)).map(|i| rrc(i as f64 / TABLE as f64 - SPAN as f64, ROLLOFF)).collect();
    let pulse = |t: f64| -> f64 {
        let u = (t + SPAN as f64) * TABLE as f64;
        if u <= 0.0 || u >= (table.len() - 1) as f64 {
            return 0.0;
        }
        let i = u.floor() as usize;
        let f = u - i as f64;
        table[i] * (1.0 - f) + table[i + 1] * f
    };
    let t_sym = 1.0 / baud;
    let body_s = syms.len() as f64 * t_sym;
    let n = ((ch.lead_s + body_s + ch.tail_s) * ch.fs) as usize;
    // Fading gains at 200 Hz, linearly interpolated.
    let grate = 200.0;
    let gn = (n as f64 / ch.fs * grate) as usize + 4;
    let gains: Vec<Vec<Complex64>> = ch
        .paths
        .iter()
        .enumerate()
        .map(|(i, p)| match ch.spread_hz {
            Some(sp) => fading(gn, grate, sp, ch.seed * 31 + i as u64 * 7 + 3).into_iter().map(|g| g * p.power.sqrt()).collect(),
            None => vec![Complex64::new(p.power.sqrt(), 0.0); gn],
        })
        .collect();
    let mut out = vec![0f32; n];
    let mut sig_p = 0.0;
    let mut sig_n = 0usize;
    let clock = 1.0 + ch.ppm * 1e-6;
    for (i, o) in out.iter_mut().enumerate() {
        let t_rx = i as f64 / ch.fs;
        let t = t_rx * clock - ch.lead_s;
        let gpos = t_rx * grate;
        let gi = gpos.floor() as usize;
        let gf = gpos - gi as f64;
        let mut y = Complex64::new(0.0, 0.0);
        for (pi, p) in ch.paths.iter().enumerate() {
            let tp = t - p.delay_s;
            let center = (tp / t_sym).round() as i64;
            let mut s = Complex64::new(0.0, 0.0);
            for k in center - SPAN..=center + SPAN {
                if k < 0 || k >= syms.len() as i64 {
                    continue;
                }
                let h = pulse(tp / t_sym - k as f64);
                let a = syms[k as usize];
                s += Complex64::new(f64::from(a.re), f64::from(a.im)) * h;
            }
            let g = gains[pi][gi] * (1.0 - gf) + gains[pi][(gi + 1).min(gn - 1)] * gf;
            y += s * g;
        }
        let ph = 2.0 * PI * (ch.carrier_hz + ch.offset_hz) * t;
        let v = (y * Complex64::new(ph.cos(), ph.sin())).re * 2f64.sqrt();
        if t >= 0.0 && t < body_s {
            sig_p += v * v;
            sig_n += 1;
        }
        *o = v as f32;
    }
    if let Some(snr) = ch.snr_db {
        let ps = sig_p / sig_n.max(1) as f64;
        let var = ps * ch.fs / (2.0 * 3000.0 * 10f64.powf(snr / 10.0));
        let sd = var.sqrt();
        let mut rng = Rng(ch.seed.wrapping_mul(0x9e37_79b9) | 1);
        for o in &mut out {
            *o += (rng.gauss() * sd) as f32;
        }
    }
    let peak = out.iter().fold(0f32, |m, v| m.max(v.abs())).max(1e-9);
    out.iter_mut().for_each(|v| *v *= 0.3 / peak);
    out
}

/// Any audio through the channel: made analytic by zeroing its negative
/// frequencies, each path delayed and faded, shifted by the offset, and
/// noise added against the power of the input's nonzero stretch. The
/// carrier and clock settings are not used.
pub(super) fn through(audio: &[f32], ch: &Channel) -> Vec<f32> {
    let lead = (ch.lead_s * ch.fs) as usize;
    let tail = (ch.tail_s * ch.fs) as usize;
    let n = (audio.len() + lead + tail).next_power_of_two();
    let mut x: Vec<Complex64> = vec![Complex64::new(0.0, 0.0); n];
    for (i, &a) in audio.iter().enumerate() {
        x[lead + i] = Complex64::new(f64::from(a), 0.0);
    }
    let mut planner = rustfft::FftPlanner::new();
    planner.plan_fft_forward(n).process(&mut x);
    for (k, v) in x.iter_mut().enumerate() {
        *v *= if k == 0 || k == n / 2 { 1.0 } else if k < n / 2 { 2.0 } else { 0.0 } / n as f64;
    }
    planner.plan_fft_inverse(n).process(&mut x);
    let grate = 200.0;
    let gn = (n as f64 / ch.fs * grate) as usize + 4;
    let mut y = vec![Complex64::new(0.0, 0.0); n];
    for (pi, p) in ch.paths.iter().enumerate() {
        let gains: Vec<Complex64> = match ch.spread_hz {
            Some(sp) => fading(gn, grate, sp, ch.seed * 31 + pi as u64 * 7 + 3).into_iter().map(|g| g * p.power.sqrt()).collect(),
            None => vec![Complex64::new(p.power.sqrt(), 0.0); gn],
        };
        let d = (p.delay_s * ch.fs).round() as usize;
        for i in d..n {
            let gpos = i as f64 / ch.fs * grate;
            let gi = gpos.floor() as usize;
            let gf = gpos - gi as f64;
            let g = gains[gi] * (1.0 - gf) + gains[(gi + 1).min(gn - 1)] * gf;
            y[i] += x[i - d] * g;
        }
    }
    let total = audio.len() + lead + tail;
    let mut out: Vec<f32> = (0..total)
        .map(|i| {
            let ph = 2.0 * PI * ch.offset_hz * i as f64 / ch.fs;
            (y[i] * Complex64::new(ph.cos(), ph.sin())).re as f32
        })
        .collect();
    if let Some(snr) = ch.snr_db {
        let ps = out[lead..lead + audio.len()].iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / audio.len().max(1) as f64;
        let sd = (ps * ch.fs / (2.0 * 3000.0 * 10f64.powf(snr / 10.0))).sqrt();
        let mut rng = Rng(ch.seed.wrapping_mul(0x9e37_79b9) | 1);
        for o in &mut out {
            *o += (rng.gauss() * sd) as f32;
        }
    }
    out
}

/// White noise alone.
pub(super) fn noise(fs: f64, secs: f64, seed: u64) -> Vec<f32> {
    let mut rng = Rng(seed | 1);
    (0..(fs * secs) as usize).map(|_| (rng.gauss() * 0.05) as f32).collect()
}
