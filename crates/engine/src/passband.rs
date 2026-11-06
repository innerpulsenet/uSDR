//! The sideband passband's low-pass, in three shapes.
//!
//! The Weaver demodulator (see [`crate::ssb`]) moves the wanted audio band
//! down until it sits symmetrically on 0 Hz, so the whole passband, and both
//! of its skirts, come from one low-pass at half the band's width. What that
//! low-pass looks like outside the band is the filter's shape:
//!
//! - **Sharp**: an elliptic filter whose skirt falls 70 dB in about 100 Hz.
//!   The most selective, and the most ringing: a click or a noise crash
//!   rings on at the band edges, and the edges' group delay peaks.
//! - **Normal**: an elliptic filter with a 300 Hz skirt to 60 dB. Selective
//!   enough to keep a neighbor 3 kHz away out, without a sharp filter's ring.
//! - **Soft**: a linear-phase FIR rolling off smoothly over 400 Hz, 70 dB
//!   down at its end (a Kaiser-windowed sinc, which reaches its floor in
//!   fewer taps than a windowed raised cosine). Its ring is its own few
//!   milliseconds of tails, and no frequency is delayed more than another,
//!   so speech and transients keep their shape. It costs selectivity, and
//!   with a low edge near the carrier some of the opposite sideband.
//!
//! In every shape the skirt is a fixed number of hertz, whatever the width,
//! so a shape sounds the same on a narrow filter as on a wide one, and it
//! lies outside the edges: the edges the operator sets are the flat
//! passband (within 0.5 dB for the elliptic shapes, a few hundredths for
//! the FIR).
//!
//! The elliptic filters are designed here from the analog prototype
//! (S. J. Orfanidis, "Lecture Notes on Elliptic Filter Design", 2006), with
//! the order chosen to meet the skirt and stopband, and run as second-order
//! sections in f64: a 12th-order design's poles sit close to the unit
//! circle, where f32 coefficients and state would move them.

use num_complex::{Complex32, Complex64};
use serde::{Deserialize, Serialize};
use std::f64::consts::PI;

/// How the passband's skirt falls away. See the module notes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum FilterShape {
    Sharp,
    #[default]
    Normal,
    Soft,
}

impl FilterShape {
    pub const ALL: [FilterShape; 3] = [FilterShape::Sharp, FilterShape::Normal, FilterShape::Soft];

    pub fn wire(self) -> &'static str {
        match self {
            FilterShape::Sharp => "sharp",
            FilterShape::Normal => "normal",
            FilterShape::Soft => "soft",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.wire().eq_ignore_ascii_case(s))
    }

    /// How far past each edge the skirt reaches the stopband, Hz.
    pub fn skirt_hz(self) -> f64 {
        match self {
            FilterShape::Sharp => SHARP.skirt_hz,
            FilterShape::Normal => NORMAL.skirt_hz,
            FilterShape::Soft => SOFT_ROLLOFF_HZ,
        }
    }

    /// The stopband floor the shape is designed to, dB.
    pub fn stopband_db(self) -> f64 {
        match self {
            FilterShape::Sharp => SHARP.stop_db,
            FilterShape::Normal => NORMAL.stop_db,
            FilterShape::Soft => SOFT_STOP_DB,
        }
    }
}

/// An elliptic shape's specification.
struct Elliptic {
    ripple_db: f64,
    stop_db: f64,
    skirt_hz: f64,
}

const SHARP: Elliptic = Elliptic { ripple_db: 0.5, stop_db: 70.0, skirt_hz: 100.0 };
const NORMAL: Elliptic = Elliptic { ripple_db: 0.5, stop_db: 60.0, skirt_hz: 300.0 };
/// Elliptic orders are even (whole second-order sections) and held to this
/// range: below 4 the filter is barely a filter; past 12 the poles crowd the
/// unit circle and the ring outlasts a syllable. A wide sharp filter that
/// would need more gets a slightly wider skirt instead.
const MIN_ORDER: usize = 4;
const MAX_ORDER: usize = 12;

/// The soft shape: its whole roll-off, and the stopband floor it reaches.
const SOFT_ROLLOFF_HZ: f64 = 400.0;
const SOFT_STOP_DB: f64 = 70.0;

/// A low-pass for the Weaver baseband, run on both rails at once.
pub enum Lowpass {
    Iir(Vec<Section>),
    Fir(Fir),
}

impl Lowpass {
    /// The low-pass for a passband `half_hz` either side of 0 Hz, at `fs`.
    pub fn design(shape: FilterShape, half_hz: f64, fs: f64) -> Self {
        // Stay well under Nyquist whatever is asked for.
        let half = half_hz.clamp(10.0, fs * 0.2);
        match shape {
            FilterShape::Sharp => Lowpass::Iir(elliptic(&SHARP, half, fs)),
            FilterShape::Normal => Lowpass::Iir(elliptic(&NORMAL, half, fs)),
            // Kaiser's length formula runs a dB or so short; design with margin.
            FilterShape::Soft => Lowpass::Fir(Fir::kaiser_lowpass(half, SOFT_ROLLOFF_HZ.min(fs * 0.1), SOFT_STOP_DB + 3.0, fs)),
        }
    }

    #[inline]
    pub fn process(&mut self, x: Complex32) -> Complex32 {
        match self {
            Lowpass::Iir(sections) => {
                let mut v = Complex64::new(x.re as f64, x.im as f64);
                for s in sections.iter_mut() {
                    v = s.process(v);
                }
                Complex32::new(v.re as f32, v.im as f32)
            }
            Lowpass::Fir(f) => f.process(x),
        }
    }

    pub fn reset(&mut self) {
        match self {
            Lowpass::Iir(sections) => sections.iter_mut().for_each(Section::reset),
            Lowpass::Fir(f) => f.reset(),
        }
    }

    /// The filter's gain at `f` Hz, computed from its coefficients.
    pub fn response(&self, f: f64, fs: f64) -> f64 {
        let w = 2.0 * PI * f / fs;
        let z1 = Complex64::from_polar(1.0, -w);
        match self {
            Lowpass::Iir(sections) => sections.iter().map(|s| s.response(z1)).product::<f64>(),
            Lowpass::Fir(f) => {
                let mut acc = Complex64::new(0.0, 0.0);
                let mut zk = Complex64::new(1.0, 0.0);
                for &h in &f.taps {
                    acc += zk * h as f64;
                    zk *= z1;
                }
                acc.norm()
            }
        }
    }

    /// The elliptic order, or the FIR's length.
    pub fn size(&self) -> usize {
        match self {
            Lowpass::Iir(s) => 2 * s.len(),
            Lowpass::Fir(f) => f.taps.len(),
        }
    }
}

/// One second-order section, transposed direct form II, real coefficients
/// on a complex signal.
#[derive(Clone, Debug)]
pub struct Section {
    b: [f64; 3],
    a: [f64; 2],
    z1: Complex64,
    z2: Complex64,
}

impl Section {
    /// The bilinear transform of `(n2 s² + n1 s + n0) / (s² + d1 s + d0)`,
    /// an analog section normalized to a 1 rad/s corner, placed at the
    /// digital corner whose prewarped frequency is `warp` = tan(π fc / fs).
    fn bilinear(n: [f64; 3], d: [f64; 2], warp: f64) -> Self {
        let (w, w2) = (warp, warp * warp);
        let [n2, n1, n0] = n;
        let [d1, d0] = d;
        let b0 = n2 + n1 * w + n0 * w2;
        let b1 = 2.0 * (n0 * w2 - n2);
        let b2 = n2 - n1 * w + n0 * w2;
        let a0 = 1.0 + d1 * w + d0 * w2;
        let a1 = 2.0 * (d0 * w2 - 1.0);
        let a2 = 1.0 - d1 * w + d0 * w2;
        Section { b: [b0 / a0, b1 / a0, b2 / a0], a: [a1 / a0, a2 / a0], z1: Complex64::default(), z2: Complex64::default() }
    }

    #[inline]
    fn process(&mut self, x: Complex64) -> Complex64 {
        let y = x * self.b[0] + self.z1;
        self.z1 = x * self.b[1] - y * self.a[0] + self.z2;
        self.z2 = x * self.b[2] - y * self.a[1];
        y
    }

    fn reset(&mut self) {
        self.z1 = Complex64::default();
        self.z2 = Complex64::default();
    }

    fn response(&self, z1: Complex64) -> f64 {
        let z2 = z1 * z1;
        let num = z1 * self.b[1] + z2 * self.b[2] + self.b[0];
        let den = z1 * self.a[0] + z2 * self.a[1] + 1.0;
        (num / den).norm()
    }
}

/// A linear-phase FIR on both rails. The history is kept twice over, so the
/// newest `taps.len()` samples are always one contiguous run.
pub struct Fir {
    taps: Vec<f32>,
    re: Vec<f32>,
    im: Vec<f32>,
    pos: usize,
}

impl Fir {
    /// A Kaiser-windowed sinc: flat to `flat_hz`, then an S-shaped
    /// roll-off to the stopband floor at `flat_hz + rolloff_hz` (Kaiser's
    /// design formulas for the length and the window).
    fn kaiser_lowpass(flat_hz: f64, rolloff_hz: f64, stop_db: f64, fs: f64) -> Self {
        let fc = flat_hz + rolloff_hz / 2.0;
        let dw = 2.0 * PI * rolloff_hz / fs;
        let half = (((stop_db - 8.0) / (2.285 * dw) / 2.0).ceil() as usize).max(4);
        let n = 2 * half + 1;
        let beta = 0.1102 * (stop_db - 8.7);
        let i0 = |x: f64| {
            let (mut sum, mut term, mut k) = (1.0, 1.0, 1.0);
            while term > 1e-12 * sum {
                term *= (x / (2.0 * k)).powi(2);
                sum += term;
                k += 1.0;
            }
            sum
        };
        let norm = i0(beta);
        let mut taps: Vec<f64> = (0..n)
            .map(|i| {
                let k = i as f64 - half as f64;
                let sinc = if k == 0.0 { 2.0 * fc / fs } else { (2.0 * PI * fc * k / fs).sin() / (PI * k) };
                let r = k / half as f64;
                sinc * i0(beta * (1.0 - r * r).max(0.0).sqrt()) / norm
            })
            .collect();
        let dc: f64 = taps.iter().sum();
        taps.iter_mut().for_each(|t| *t /= dc);
        Fir { taps: taps.into_iter().map(|t| t as f32).collect(), re: vec![0.0; 2 * n], im: vec![0.0; 2 * n], pos: 0 }
    }

    #[inline]
    fn process(&mut self, x: Complex32) -> Complex32 {
        let n = self.taps.len();
        self.re[self.pos] = x.re;
        self.re[self.pos + n] = x.re;
        self.im[self.pos] = x.im;
        self.im[self.pos + n] = x.im;
        self.pos += 1;
        if self.pos == n {
            self.pos = 0;
        }
        // Oldest first from `pos`; the taps are symmetric, so their order
        // does not matter.
        Complex32::new(dot(&self.taps, &self.re[self.pos..self.pos + n]), dot(&self.taps, &self.im[self.pos..self.pos + n]))
    }

    fn reset(&mut self) {
        self.re.fill(0.0);
        self.im.fill(0.0);
        self.pos = 0;
    }
}

/// A dot product in eight lanes, which the compiler turns into vector code.
#[inline]
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0.0f32; 8];
    let (ac, bc) = (a.chunks_exact(8), b.chunks_exact(8));
    let (ar, br) = (ac.remainder(), bc.remainder());
    for (x, y) in ac.zip(bc) {
        for l in 0..8 {
            acc[l] += x[l] * y[l];
        }
    }
    let tail: f32 = ar.iter().zip(br).map(|(x, y)| x * y).sum();
    acc.iter().sum::<f32>() + tail
}

/// Complete elliptic integral of the first kind, K(k), by the AGM.
fn ellip_k(k: f64) -> f64 {
    let (mut a, mut b) = (1.0, (1.0 - k * k).max(0.0).sqrt());
    for _ in 0..40 {
        if (a - b).abs() < 1e-15 * a {
            break;
        }
        (a, b) = ((a + b) / 2.0, (a * b).sqrt());
    }
    PI / (2.0 * a)
}

/// The nome of modulus `k`: exp(−π K′/K).
fn nome(k: f64) -> f64 {
    (-PI * ellip_k((1.0 - k * k).sqrt()) / ellip_k(k)).exp()
}

/// The modulus whose nome is `q` (Jacobi's product).
fn modulus_of_nome(q: f64) -> f64 {
    let mut p = 1.0;
    for n in 1..200 {
        let a = q.powi(2 * n);
        let b = q.powi(2 * n - 1);
        if b < 1e-17 {
            break;
        }
        p *= ((1.0 + a) / (1.0 + b)).powi(4);
    }
    (4.0 * q.sqrt() * p).min(1.0)
}

/// The descending Landen moduli of `k`.
fn landen(k: f64) -> Vec<f64> {
    let mut v = Vec::new();
    let mut k = k;
    for _ in 0..12 {
        k = (k / (1.0 + (1.0 - k * k).sqrt())).powi(2);
        v.push(k);
        if k < 1e-16 {
            break;
        }
    }
    v
}

/// cd(uK, k) for complex `u`, by ascending Landen.
fn cde(u: Complex64, k: f64) -> Complex64 {
    let mut w = (u * (PI / 2.0)).cos();
    for &v in landen(k).iter().rev() {
        w = w * (1.0 + v) / (w * w * v + 1.0);
    }
    w
}

/// The inverse of sn: the `u` with sn(uK, k) = w (sn(uK) = cd((1 − u)K)).
fn asne(w: Complex64, k: f64) -> Complex64 {
    let v = landen(k);
    let mut w = w;
    let mut prev = k;
    for &vn in &v {
        w = w / ((Complex64::new(1.0, 0.0) - w * w * (prev * prev)).sqrt() + 1.0) * (2.0 / (1.0 + vn));
        prev = vn;
    }
    // acde = 2 acos(w)/π, and asne = 1 − acde.
    Complex64::new(1.0, 0.0) - w.acos() * (2.0 / PI)
}

/// The elliptic low-pass for a spec, as sections at the corner `half_hz`.
fn elliptic(spec: &Elliptic, half_hz: f64, fs: f64) -> Vec<Section> {
    let ep = (10f64.powf(spec.ripple_db / 10.0) - 1.0).sqrt();
    let es = (10f64.powf(spec.stop_db / 10.0) - 1.0).sqrt();
    // Selectivity k = passband edge / stopband edge, in prewarped
    // frequency, so the skirt lands where it is meant to after the
    // bilinear transform.
    let warp = |f: f64| (PI * f.min(fs * 0.49) / fs).tan();
    let k_want = warp(half_hz) / warp(half_hz + spec.skirt_hz);
    let k1_want = ep / es;
    // The degree equation in nomes: q1 = q^N.
    let n_exact = nome(k1_want).ln() / nome(k_want).ln();
    let n_even = ((n_exact / 2.0).ceil() as usize * 2).clamp(MIN_ORDER, MAX_ORDER);
    let k = if n_exact <= MAX_ORDER as f64 {
        // Keep the skirt; the extra order deepens the stopband.
        k_want
    } else {
        // Out of order to spend: keep the stopband, widen the skirt.
        modulus_of_nome(nome(k1_want).powf(1.0 / n_even as f64))
    };
    let k1 = modulus_of_nome(nome(k).powi(n_even as i32));
    let n = n_even as f64;
    // Zeros on the jΩ axis at 1/(k cd(u_i K, k)); poles from the shifted
    // argument, both for the passband edge at Ω = 1.
    let v0 = (asne(Complex64::new(0.0, 1.0 / ep), k1) * Complex64::new(0.0, -1.0) / n).re;
    let w = warp(half_hz);
    let mut sections: Vec<Section> = (1..=n_even / 2)
        .map(|i| {
            let u = (2 * i - 1) as f64 / n;
            let zeta = cde(Complex64::new(u, 0.0), k).re;
            let zero = 1.0 / (k * zeta);
            let p = Complex64::new(0.0, 1.0) * cde(Complex64::new(u, -v0), k);
            let (sig, mag2) = (p.re.abs(), p.norm_sqr());
            // Unity gain at DC per section: (s² + z²)·|p|²/z² over the poles.
            let g = mag2 / (zero * zero);
            Section::bilinear([g, 0.0, g * zero * zero], [2.0 * sig, mag2], w)
        })
        .collect();
    // Even order: DC sits at the bottom of the ripple. Lift the whole so the
    // ripple peaks at 0 dB and the edges sit −ripple under it.
    let lift = 1.0 / (1.0 + ep * ep).sqrt();
    if let Some(first) = sections.first_mut() {
        for b in &mut first.b {
            *b *= lift;
        }
    }
    // Lowest Q first, so the most resonant section sees an already
    // band-limited signal.
    sections.sort_by(|a, b| a.a[1].total_cmp(&b.a[1]));
    sections
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: [f64; 3] = [48_077.0, 48_000.0, 24_038.5];
    /// Half-widths from a 200 Hz filter to a 5 kHz one.
    const HALVES: [f64; 5] = [100.0, 600.0, 1_250.0, 2_000.0, 2_500.0];

    fn db(x: f64) -> f64 {
        20.0 * x.max(1e-12).log10()
    }

    /// Peak passband gain, and the worst gain inside the band.
    fn passband(lp: &Lowpass, half: f64, fs: f64) -> (f64, f64) {
        let pts: Vec<f64> = (0..=200).map(|i| db(lp.response(half * i as f64 / 200.0, fs))).collect();
        (pts.iter().cloned().fold(f64::MIN, f64::max), pts.iter().cloned().fold(f64::MAX, f64::min))
    }

    /// Worst (highest) gain from `from` Hz out to well past it.
    fn stopband(lp: &Lowpass, from: f64, fs: f64) -> f64 {
        (0..=2_000).map(|i| db(lp.response(from + (fs / 2.0 - from) * i as f64 / 2_000.0, fs))).fold(f64::MIN, f64::max)
    }

    #[test]
    fn every_shape_keeps_its_passband_and_reaches_its_stopband_at_every_width() {
        for fs in FS {
            for half in HALVES {
                for shape in FilterShape::ALL {
                    let lp = Lowpass::design(shape, half, fs);
                    let (peak, worst) = passband(&lp, half, fs);
                    let skirt = shape.skirt_hz();
                    let stop = stopband(&lp, half + skirt * 1.05, fs);
                    eprintln!("fs {fs} half {half} {shape:?} (size {}): passband {worst:.2}..{peak:.2} dB, stopband {stop:.1} dB", lp.size());
                    assert!(peak.abs() < 0.05, "{shape:?} fs {fs} half {half}: passband peaks at {peak:.2} dB");
                    assert!(worst > -0.6, "{shape:?} fs {fs} half {half}: passband sags to {worst:.2} dB");
                    // A sharp filter as wide as 5 kHz runs out of order and
                    // trades a little of its skirt; it still holds its floor
                    // a few hertz further out.
                    let stop = if shape == FilterShape::Sharp && half >= 2_000.0 { stopband(&lp, half + skirt * 1.3, fs) } else { stop };
                    assert!(stop < -shape.stopband_db() + 0.5, "{shape:?} fs {fs} half {half}: stopband only {stop:.1} dB");
                }
            }
        }
    }

    #[test]
    fn the_skirts_are_ordered_sharp_normal_soft() {
        let fs = 48_000.0;
        let half = 1_250.0;
        // 150 Hz past the edge: sharp is already in its stopband, normal is
        // on its way down, soft has barely begun.
        let at = |s: FilterShape| db(Lowpass::design(s, half, fs).response(half + 150.0, fs));
        let (sharp, normal, soft) = (at(FilterShape::Sharp), at(FilterShape::Normal), at(FilterShape::Soft));
        eprintln!("150 Hz past the edge: sharp {sharp:.1}, normal {normal:.1}, soft {soft:.1} dB");
        assert!(sharp < -65.0 && sharp < normal - 20.0 && normal < soft - 3.0);
    }

    /// The ring after a tone at the band edge stops: how long its envelope
    /// takes to fall 60 dB.
    fn ring_ms(shape: FilterShape) -> f64 {
        let fs = 48_000.0;
        let half = 1_250.0;
        let mut lp = Lowpass::design(shape, half, fs);
        let on = (0.3 * fs) as usize;
        let mut env = Vec::new();
        for i in 0..(0.6 * fs) as usize {
            let x = if i < on { Complex32::from_polar(1.0, std::f32::consts::TAU * (half * 0.95) as f32 * i as f32 / fs as f32) } else { Complex32::new(0.0, 0.0) };
            env.push(lp.process(x).norm() as f64);
        }
        let level = env[on - 100];
        let last = env.iter().rposition(|&e| e > level * 1e-3).unwrap_or(on);
        (last.saturating_sub(on)) as f64 * 1e3 / fs
    }

    #[test]
    fn the_sharper_the_skirt_the_longer_the_ring() {
        let (sharp, normal, soft) = (ring_ms(FilterShape::Sharp), ring_ms(FilterShape::Normal), ring_ms(FilterShape::Soft));
        eprintln!("ring to −60 dB after an edge tone: sharp {sharp:.1} ms, normal {normal:.1} ms, soft {soft:.1} ms");
        assert!(sharp > normal && normal > soft, "sharp {sharp:.1}, normal {normal:.1}, soft {soft:.1}");
        // The FIR's tails are its whole ring: they end with the filter.
        assert!(soft < 12.0, "soft rang {soft:.1} ms");
    }

    #[test]
    fn the_filters_run_as_designed() {
        // A tone in the band and one in the stopband, through the running
        // filter, agree with the response computed from the coefficients.
        let fs = 48_000.0;
        for shape in FilterShape::ALL {
            let mut lp = Lowpass::design(shape, 1_250.0, fs);
            for f in [800.0, 1_250.0 + shape.skirt_hz() + 200.0] {
                lp.reset();
                let n = fs as usize;
                let mut out = Vec::with_capacity(n);
                for i in 0..n {
                    out.push(lp.process(Complex32::from_polar(1.0, std::f32::consts::TAU * f as f32 * i as f32 / fs as f32)));
                }
                let tail = &out[n / 2..];
                let amp = (tail.iter().map(|c| c.norm_sqr() as f64).sum::<f64>() / tail.len() as f64).sqrt();
                let want = lp.response(f, fs);
                assert!((db(amp) - db(want)).abs() < 1.0 || (db(amp) < -55.0 && db(want) < -55.0), "{shape:?} at {f} Hz: ran {:.1} dB, designed {:.1} dB", db(amp), db(want));
            }
        }
    }

    #[test]
    fn shapes_round_trip_by_name() {
        for s in FilterShape::ALL {
            assert_eq!(FilterShape::parse(s.wire()), Some(s));
        }
        assert_eq!(FilterShape::parse("SOFT"), Some(FilterShape::Soft));
        assert_eq!(FilterShape::parse("brick"), None);
    }
}
