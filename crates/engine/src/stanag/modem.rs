//! The parts every serial-tone receiver here shares: the front end that
//! turns sideband audio into complex baseband around 1800 Hz, the
//! resampler that lays that out at two samples a symbol, a stream with
//! absolute sample numbers so detectors and receivers can point into it,
//! the decision-feedback equalizer, and the constellations with their bit
//! labels.
//!
//! The equalizer is fractionally spaced (two taps a symbol) with symbol-
//! spaced feedback, adapted by recursive least squares. HF multipath puts
//! echoes several symbols apart that swap places as they fade, and least
//! mean squares cannot follow that at 2400 baud; RLS converges within the
//! 80-symbol sync of a STANAG 4285 frame and tracks a 1 Hz Doppler spread
//! with a memory of about 40 ms. A second-order phase loop on its output
//! takes out what remains of the carrier offset after acquisition, and the
//! centroid of the forward taps shows the symbol clock drifting, which is
//! corrected a half symbol at a time by sliding the window and the taps
//! together.

use num_complex::Complex32;
use scannerd_dsp::{DecimFir, Nco};
use std::f32::consts::PI;

pub(super) type C32 = Complex32;

/// The carrier of both NATO serial-tone families.
pub(super) const CARRIER_HZ: f32 = 1800.0;
/// Rate the front end decimates to, at least: twice the 2400 Hz symbol
/// rate plus room for the filter. The real rate is the input's divided by
/// a whole number, whatever that comes to.
const FRONT_MIN_RATE: f32 = 9_000.0;
/// The kept band either side of 1800 Hz: the 2400 Bd signal's 1.2 kHz,
/// mistuning, and the 2G ALE tones down to 750 Hz.
const FRONT_CUTOFF_HZ: f32 = 1_875.0;

/// Real sideband audio to complex baseband centered on 1800 Hz.
pub(super) struct FrontEnd {
    nco: Nco,
    fir: DecimFir,
    fs1: f32,
    cbuf: Vec<C32>,
    mixed: Vec<C32>,
}

impl FrontEnd {
    pub(super) fn new(fs: f32) -> Self {
        let decim = ((fs / FRONT_MIN_RATE).floor() as usize).max(1);
        let mut nco = Nco::new();
        nco.set_freq(f64::from(CARRIER_HZ), f64::from(fs));
        // A transition of about a kilohertz: the real input's mirror image
        // begins 2.4 kHz below the new zero and must be gone.
        let ntaps = ((5.5 * fs / 1000.0) as usize) | 1;
        Self { nco, fir: DecimFir::new(FRONT_CUTOFF_HZ, fs, decim, ntaps), fs1: fs / decim as f32, cbuf: Vec::new(), mixed: Vec::new() }
    }

    pub(super) fn rate(&self) -> f32 {
        self.fs1
    }

    pub(super) fn process(&mut self, audio: &[f32], out: &mut Vec<C32>) {
        self.cbuf.clear();
        self.cbuf.extend(audio.iter().map(|&a| C32::new(a, 0.0)));
        self.nco.mix(&self.cbuf, &mut self.mixed);
        self.fir.process(&self.mixed, out);
    }
}

/// Resamples to a fixed nominal output rate by cubic interpolation, and
/// levels the output to unit mean power with a slow AGC.
///
/// A whisper of white noise, 40 dB down, is added after the leveling. At
/// two samples a symbol the band above the signal is empty, and on a
/// clean input the equalizer's taps drift in that empty band unchecked
/// until they blow up; the noise gives them something to hold to, at a
/// cost no HF signal will notice.
pub(super) struct Resampler {
    step: f64,
    /// Position of the next output, in input samples from `hist[0]`.
    pos: f64,
    hist: Vec<C32>,
    power: f32,
    count: u32,
    agc_alpha: f32,
    dither: u64,
}

const DITHER: f32 = 0.007;

impl Resampler {
    pub(super) fn new(fs_in: f32, fs_out: f32) -> Self {
        Self {
            step: f64::from(fs_in) / f64::from(fs_out),
            pos: 1.0,
            hist: Vec::new(),
            power: 0.0,
            count: 0,
            agc_alpha: 1.0 / (0.5 * fs_out),
            dither: 0x9e37_79b9_7f4a_7c15,
        }
    }

    pub(super) fn process(&mut self, input: &[C32], out: &mut Vec<C32>) {
        self.hist.extend_from_slice(input);
        while self.pos + 2.0 < self.hist.len() as f64 {
            let i = self.pos.floor() as usize;
            let mu = (self.pos - i as f64) as f32;
            let (xm, x0, x1, x2) = (self.hist[i - 1], self.hist[i], self.hist[i + 1], self.hist[i + 2]);
            // Cubic Lagrange through the four samples around the point.
            let c0 = -mu * (mu - 1.0) * (mu - 2.0) / 6.0;
            let c1 = (mu + 1.0) * (mu - 1.0) * (mu - 2.0) / 2.0;
            let c2 = -(mu + 1.0) * mu * (mu - 2.0) / 2.0;
            let c3 = (mu + 1.0) * mu * (mu - 1.0) / 6.0;
            let y = xm * c0 + x0 * c1 + x1 * c2 + x2 * c3;
            let p = y.norm_sqr();
            self.count = self.count.saturating_add(1);
            let a = self.agc_alpha.max(1.0 / self.count as f32);
            self.power += a * (p - self.power);
            let mut d = [0f32; 2];
            for v in &mut d {
                // Sum of two uniforms: near enough Gaussian for this.
                self.dither ^= self.dither << 13;
                self.dither ^= self.dither >> 7;
                self.dither ^= self.dither << 17;
                let a = (self.dither >> 40) as f32 / (1u64 << 24) as f32;
                let b = (self.dither & 0xff_ffff) as f32 / (1u64 << 24) as f32;
                *v = (a + b - 1.0) * DITHER * 2.45;
            }
            out.push(y / self.power.max(1e-30).sqrt() + C32::new(d[0], d[1]));
            self.pos += self.step;
        }
        let drop = (self.pos.floor() as usize).saturating_sub(1);
        if drop > 0 {
            self.hist.drain(..drop);
            self.pos -= drop as f64;
        }
    }
}

/// Samples with absolute numbering: index `base` is `buf[0]`.
pub(super) struct Stream {
    buf: Vec<C32>,
    base: u64,
}

impl Stream {
    pub(super) fn new() -> Self {
        Self { buf: Vec::new(), base: 0 }
    }

    pub(super) fn extend(&mut self, x: &[C32]) {
        self.buf.extend_from_slice(x);
    }

    /// One past the newest sample.
    pub(super) fn end(&self) -> u64 {
        self.base + self.buf.len() as u64
    }

    pub(super) fn start(&self) -> u64 {
        self.base
    }

    /// `n` samples from `at`, if all are still held.
    pub(super) fn slice(&self, at: u64, n: usize) -> Option<&[C32]> {
        let i = at.checked_sub(self.base)? as usize;
        self.buf.get(i..i + n)
    }

    /// Forget what is older than `keep_from`.
    pub(super) fn trim(&mut self, keep_from: u64) {
        if keep_from > self.base + 8192 {
            let n = ((keep_from - self.base) as usize).min(self.buf.len());
            self.buf.drain(..n);
            self.base += n as u64;
        }
    }
}

/// Forward taps, two a symbol: twelve symbols, room for HF echoes 5 ms
/// apart either side of the strongest path.
pub(super) const NF: usize = 24;
/// Of the forward taps, how many look at samples before the reference.
pub(super) const PRE: usize = 12;
/// Feedback taps, one a symbol.
const NB: usize = 8;
const N: usize = NF + NB;
/// RLS forgetting factor until the constellation is known: a memory of
/// 100 symbols. See [`lambda_for`].
const LAMBDA: f32 = 0.99;

/// The forgetting factor for a constellation. A short memory follows a
/// fading channel closely but averages less noise: 8-PSK, which needs a
/// clean channel and runs where the noise is low, gets 40 symbols; BPSK,
/// sent when the channel is poor, keeps 100. Tried on the CCIR poor
/// channel, 2 ms and 1 Hz.
pub(super) fn lambda_for(order: usize) -> f32 {
    match order {
        8 => 0.975,
        4 => 0.985,
        _ => 0.99,
    }
}
/// Starting inverse correlation, for inputs of unit power.
const P0: f32 = 10.0;

#[derive(Clone)]
pub(super) struct Dfe {
    w: [C32; N],
    p: Vec<C32>,
    u: [C32; N],
    pu: [C32; N],
    /// Past symbol decisions (or known symbols), newest first.
    fb: [C32; NB],
    /// Carrier phase taken out of the forward input, and its step per
    /// symbol, radians.
    phase: f32,
    freq: f32,
    updates: u32,
    lambda: f32,
}

/// Phase loop gains: about 5 Hz of loop bandwidth at 2400 symbols a second.
const PLL_KP: f32 = 0.01;
const PLL_KI: f32 = 4.0e-5;

impl Dfe {
    pub(super) fn new(freq_rad_per_symbol: f32) -> Self {
        let mut d = Self {
            w: [C32::default(); N],
            p: vec![C32::default(); N * N],
            u: [C32::default(); N],
            pu: [C32::default(); N],
            fb: [C32::default(); NB],
            phase: 0.0,
            freq: freq_rad_per_symbol,
            updates: 0,
            lambda: LAMBDA,
        };
        d.reset_p();
        d
    }

    fn reset_p(&mut self) {
        self.p.fill(C32::default());
        for i in 0..N {
            self.p[i * N + i] = C32::new(P0, 0.0);
        }
    }

    pub(super) fn set_lambda(&mut self, lambda: f32) {
        self.lambda = lambda;
    }

    /// Start a block over with the taps kept: the phase back to zero, the
    /// loop's frequency to `freq`, the feedback emptied. For a second pass
    /// over a training sequence.
    pub(super) fn restart(&mut self, freq: f32) {
        self.phase = 0.0;
        self.freq = freq;
        self.fb = [C32::default(); NB];
    }

    /// The carrier offset the loop has settled on, radians a symbol.
    pub(super) fn freq(&self) -> f32 {
        self.freq
    }

    /// The equalizer's output for one symbol from its `NF` forward samples,
    /// the reference sample at `PRE`.
    pub(super) fn filter(&mut self, x: &[C32]) -> C32 {
        let rot = C32::from_polar(1.0, -self.phase);
        for (u, &x) in self.u[..NF].iter_mut().zip(x) {
            *u = x * rot;
        }
        self.u[NF..].copy_from_slice(&self.fb);
        let mut y = C32::default();
        for i in 0..N {
            y += self.w[i].conj() * self.u[i];
        }
        y
    }

    /// Finish a symbol: `y` from [`Self::filter`], `d` the symbol it was
    /// (known, or decided). Adapts the taps if `adapt`, turns the phase
    /// loop, and shifts `d` into the feedback.
    pub(super) fn update(&mut self, y: C32, d: C32, adapt: bool) {
        if adapt {
            self.rls(d - y);
        }
        if d.norm_sqr() > 1e-6 && y.norm_sqr() > 1e-6 {
            let err = (y * d.conj()).arg().clamp(-0.6, 0.6);
            self.freq += PLL_KI * err;
            self.phase += PLL_KP * err;
        }
        self.phase += self.freq;
        if self.phase > PI {
            self.phase -= 2.0 * PI;
        } else if self.phase < -PI {
            self.phase += 2.0 * PI;
        }
        self.fb.copy_within(0..NB - 1, 1);
        self.fb[0] = d;
    }

    fn rls(&mut self, e: C32) {
        for i in 0..N {
            let row = &self.p[i * N..(i + 1) * N];
            self.pu[i] = row.iter().zip(&self.u).map(|(p, u)| p * u).sum();
        }
        let mut den = self.lambda;
        for i in 0..N {
            den += (self.u[i].conj() * self.pu[i]).re;
        }
        if !den.is_finite() || den <= 0.0 {
            self.reset_p();
            return;
        }
        let inv_den = 1.0 / den;
        let inv_l = 1.0 / self.lambda;
        let ec = e.conj();
        for i in 0..N {
            let k = self.pu[i] * inv_den;
            self.w[i] += k * ec;
            let row = &mut self.p[i * N..(i + 1) * N];
            for (p, pu) in row.iter_mut().zip(&self.pu) {
                *p = (*p - k * pu.conj()) * inv_l;
            }
        }
        self.updates += 1;
        if self.updates.is_multiple_of(256) {
            self.tidy();
        }
    }

    /// Keep P Hermitian against rounding, and start it over if it has run
    /// away (a long fade with nothing to learn from inflates it).
    fn tidy(&mut self) {
        let mut trace = 0.0;
        for i in 0..N {
            for j in i + 1..N {
                let a = (self.p[i * N + j] + self.p[j * N + i].conj()) * 0.5;
                self.p[i * N + j] = a;
                self.p[j * N + i] = a.conj();
            }
            let d = self.p[i * N + i].re;
            self.p[i * N + i] = C32::new(d, 0.0);
            trace += d;
        }
        if !trace.is_finite() || trace > 100.0 * P0 * N as f32 || self.w.iter().any(|w| !w.re.is_finite()) {
            if self.w.iter().any(|w| !w.re.is_finite()) {
                self.w = [C32::default(); N];
            }
            self.reset_p();
        }
    }

    /// Where the forward taps' energy sits, in samples from the first.
    pub(super) fn centroid(&self) -> f32 {
        let mut s = 0.0;
        let mut m = 0.0;
        for (i, w) in self.w[..NF].iter().enumerate() {
            let p = w.norm_sqr();
            s += p;
            m += p * i as f32;
        }
        if s > 0.0 { m / s } else { PRE as f32 }
    }

    /// The window is moving one sample later (`dir` = 1) or earlier (-1):
    /// slide the forward taps and their part of P to match.
    pub(super) fn shift(&mut self, dir: i32) {
        let map = |i: usize| -> Option<usize> {
            if i >= NF {
                return Some(i);
            }
            let j = i as i32 + dir;
            (0..NF as i32).contains(&j).then_some(j as usize)
        };
        let mut w = [C32::default(); N];
        let mut p = vec![C32::default(); N * N];
        for i in 0..N {
            if let Some(oi) = map(i) {
                w[i] = self.w[oi];
                for j in 0..N {
                    if let Some(oj) = map(j) {
                        p[i * N + j] = self.p[oi * N + oj];
                    }
                }
            } else {
                p[i * N + i] = C32::new(P0, 0.0);
            }
        }
        self.w = w;
        self.p = p;
    }
}

/// Symbol clock tracking from the equalizer's forward taps: their center
/// of energy, averaged so that echoes trading strength as they fade do not
/// move the window, is kept within a sample of the window's middle.
#[derive(Default)]
pub(super) struct Clock {
    avg: Option<f32>,
}

impl Clock {
    /// Feed the taps' centroid; the way to slide the window (and the taps),
    /// or 0.
    pub(super) fn update(&mut self, centroid: f32, alpha: f32) -> i32 {
        let a = self.avg.get_or_insert(PRE as f32);
        *a += alpha * (centroid - *a);
        if *a > PRE as f32 + 1.0 {
            *a -= 1.0;
            1
        } else if *a < PRE as f32 - 1.0 {
            *a += 1.0;
            -1
        } else {
            0
        }
    }
}

/// A constellation and the bits each point stands for, first bit sent in
/// the highest place of the label.
pub(super) struct Constellation {
    pub(super) points: Vec<C32>,
    pub(super) labels: Vec<u8>,
    pub(super) bits: usize,
}

pub(super) fn psk(n: usize, k: usize) -> C32 {
    C32::from_polar(1.0, 2.0 * PI * k as f32 / n as f32)
}

impl Constellation {
    /// Points from 8-PSK phase numbers (multiples of 45 degrees), one per
    /// label, label `i` at phase `phases[i]`.
    pub(super) fn from_phases(phases: &[usize], bits: usize) -> Self {
        Self { points: phases.iter().map(|&k| psk(8, k)).collect(), labels: (0..phases.len() as u8).collect(), bits }
    }

    pub(super) fn nearest(&self, z: C32) -> usize {
        let mut best = 0;
        let mut bd = f32::MAX;
        for (i, p) in self.points.iter().enumerate() {
            let d = (z - p).norm_sqr();
            if d < bd {
                bd = d;
                best = i;
            }
        }
        best
    }

    /// Max-log likelihood ratios of the point's bits, first sent first,
    /// positive for a 0, for complex noise of variance `var`.
    pub(super) fn soft(&self, z: C32, var: f32, out: &mut Vec<f32>) {
        let mut d = [0f32; 8];
        for (i, p) in self.points.iter().enumerate() {
            d[i] = (z - p).norm_sqr();
        }
        let inv = 1.0 / var.max(1e-3);
        for b in (0..self.bits).rev() {
            let mut m0 = f32::MAX;
            let mut m1 = f32::MAX;
            for (i, &l) in self.labels.iter().enumerate() {
                if (l >> b) & 1 == 0 {
                    m0 = m0.min(d[i]);
                } else {
                    m1 = m1.min(d[i]);
                }
            }
            out.push(((m1 - m0) * inv).clamp(-50.0, 50.0));
        }
    }
}

/// The squared distance from a data symbol to its decision under which
/// the decision is trusted to adapt the equalizer: 60% of the way to the
/// decision boundary, squared. In a fade, wrong decisions would teach the
/// taps the wrong channel.
pub(super) fn confident_error(order: usize) -> f32 {
    let half_gap = (PI / order as f32).sin();
    (0.6 * half_gap).powi(2)
}

/// A carrier offset in radians a symbol to hertz.
pub(super) fn rad_to_hz(rad: f32, baud: f32) -> f32 {
    rad * baud / (2.0 * PI)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soft_bits_follow_the_labels() {
        // QPSK as 4285 sends it: 00 at 0, 01 at 90, 10 at 270, 11 at 180.
        let c = Constellation::from_phases(&[0, 2, 6, 4], 2);
        let mut v = Vec::new();
        c.soft(psk(8, 6), 0.1, &mut v);
        assert!(v[0] < 0.0 && v[1] > 0.0, "{v:?}");
        v.clear();
        c.soft(psk(8, 4), 0.1, &mut v);
        assert!(v[0] < 0.0 && v[1] < 0.0, "{v:?}");
    }

    #[test]
    fn rls_learns_a_two_path_channel() {
        // Symbols through h = 1 + 0.6 z^-3 (in symbols), at two samples a
        // symbol with zero between: the equalizer, trained, must leave
        // little error.
        let mut seed = 3u64;
        let mut syms = Vec::new();
        for _ in 0..3000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            syms.push(psk(8, (seed >> 40) as usize % 8));
        }
        let mut x = vec![C32::default(); 2 * syms.len() + 64];
        for (k, &s) in syms.iter().enumerate() {
            x[2 * k + 20] += s;
            x[2 * k + 20 + 6] += s * C32::new(0.0, 0.6);
        }
        let mut dfe = Dfe::new(0.0);
        let mut err = 0.0;
        for (k, &s) in syms.iter().enumerate() {
            let at = 2 * k + 20 - PRE;
            let y = dfe.filter(&x[at..at + NF]);
            if k > 2000 {
                err += (y - s).norm_sqr();
            }
            dfe.update(y, s, true);
        }
        assert!(err / 1000.0 < 0.01, "mse {}", err / 1000.0);
    }
}
