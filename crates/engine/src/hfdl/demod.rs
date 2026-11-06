//! One burst, from samples to bytes: an equalizer adapted by recursive
//! least squares, soft demapping, deinterleaving and the Viterbi decoder.
//!
//! The equalizer comes in two shapes. With feedback taps it is a decision
//! feedback equalizer, which cancels the echo of symbols already decided
//! without the noise a linear filter adds where two paths nearly cancel;
//! that is what 4-PSK and 8-PSK need on a fading two-path channel. But
//! near the decoding threshold a tenth or more of the decisions are wrong,
//! and fed back they make more: there the purely linear equalizer does
//! better (on the test channel, 2-PSK decodes about 3 dB lower without
//! feedback in plain noise). Which shape suits a burst is only known
//! afterwards, so the caller runs one and then, if the frame check fails,
//! the other.

use super::fec::{pack_lsb_first, viterbi};
use super::phy::{
    DATA_LEN, MODES, PREAMBLE_LEN, SCRAMBLE_LEN, SYMBOL_RATE, T_LEN, Tables, index_to_gray,
    interleave_map,
};
use num_complex::{Complex32, Complex64};
use std::f64::consts::PI;

/// Symbols of samples kept before the first A symbol and after the last
/// probe, for the feedforward filter's reach either side.
pub const LEAD_SYM: usize = 5;
pub const TAIL_SYM: usize = 8;
/// Feedforward taps, half a symbol apart: four symbols before the timing
/// reference (the strongest path when the burst was found) and six after.
/// HF paths 1 to 2 ms apart are 2 to 4 symbols, and the reference may be
/// either the earlier or the later one; the longer reach forward is for
/// a later path that grows stronger as the burst goes on. The span also
/// leaves room for the timing to drift most of a symbol over a double slot
/// burst when the sample clock is 100 ppm off.
const FF_LEFT: usize = 8;
const FF_RIGHT: usize = 12;
const NFF: usize = FF_LEFT + FF_RIGHT + 1;
/// Feedback taps, one per past decision: the later paths' echo of the
/// last symbols, with the pulse's tails.
const NFB: usize = 10;
/// Forgetting factor: a memory of about a hundred symbols (55 ms), short
/// against a fade at 1 Hz Doppler spread, long against the noise.
const LAMBDA: f64 = 0.99;
/// Weight of an update on a decided data symbol against one on a known
/// symbol. Decisions keep the equalizer moving with the fade between
/// probes, but a wrong one pulls it the wrong way, and on 8-PSK in a fade
/// many are wrong; at a fifth, the probes stay in charge.
const DD_WEIGHT: f64 = 0.2;
/// Starting inverse correlation matrix, a multiple of the identity.
const P0: f64 = 10.0;
/// Carrier loop gains (per symbol), on the phase error after the
/// equalizer. The equalizer follows phase by itself, but with a lag that
/// grows with any frequency error left over from the preamble; the
/// loop's integrator takes that error out.
const PLL_ALPHA: f64 = 0.004;
const PLL_BETA: f64 = 4e-5;
/// Soft values are capped here, so one badly wrong but confident chip
/// cannot outweigh its neighbors in the Viterbi decoder.
const LLR_MAX: f32 = 24.0;
/// Symbols in each piece of the fine frequency fit.
const FINE_SEG: usize = 24;
/// White noise added to the equalizer's input, as a share of the signal's
/// power (30 dB down). The matched filter leaves nothing between 1.2 and
/// 1.8 kHz, signal or noise, so half-symbol samples have a hole in their
/// spectrum where least squares has nothing to fit: the taps wander there
/// without bound until rounding tips the recursion over. A little noise
/// fills the hole, which is a Tikhonov regularization of the fit, and costs
/// nothing at the signal to noise ratios the data needs.
const DITHER: f64 = 1e-3;

pub struct Demod {
    pub bytes: Vec<u8>,
    /// Signal to noise in 3 kHz, dB.
    pub snr_db: f32,
    /// Frequency the carrier loop took out on top of the preamble's
    /// estimate, Hz.
    pub residual_hz: f32,
}

/// Exponentially weighted recursive least squares (Haykin's form): output
/// `w^H u`, gain from the inverse correlation matrix `P`.
struct Rls {
    n: usize,
    w: Vec<Complex64>,
    p: Vec<Complex64>,
    pi: Vec<Complex64>,
}

impl Rls {
    fn new(n: usize) -> Self {
        let mut w = vec![Complex64::new(0.0, 0.0); n];
        w[FF_LEFT] = Complex64::new(1.0, 0.0);
        let mut s = Self {
            n,
            w,
            p: Vec::new(),
            pi: vec![Complex64::new(0.0, 0.0); n],
        };
        s.reset_p();
        s
    }

    fn reset_p(&mut self) {
        let n = self.n;
        self.p = vec![Complex64::new(0.0, 0.0); n * n];
        for i in 0..n {
            self.p[i * n + i] = Complex64::new(P0, 0.0);
        }
    }

    fn output(&self, u: &[Complex64]) -> Complex64 {
        self.w.iter().zip(u.iter()).map(|(w, x)| w.conj() * x).sum()
    }

    /// `e` is the a priori error, wanted minus output.
    fn update(&mut self, u: &[Complex64], e: Complex64) {
        let n = self.n;
        for i in 0..n {
            let row = &self.p[i * n..(i + 1) * n];
            self.pi[i] = row.iter().zip(u.iter()).map(|(p, x)| p * x).sum();
        }
        let den: f64 = LAMBDA
            + u.iter()
                .zip(self.pi.iter())
                .map(|(x, p)| (x.conj() * p).re)
                .sum::<f64>();
        if !den.is_finite() || den <= 1e-9 {
            self.reset_p();
            return;
        }
        let inv_l = 1.0 / LAMBDA;
        // P stays Hermitian only if it is kept so: rounding alone makes it
        // drift apart a little each update, and a few thousand updates in,
        // the equalizer comes apart. So one triangle is computed and the
        // other mirrored from it.
        for i in 0..n {
            let k = self.pi[i] / den;
            self.w[i] += k * e.conj();
            for j in i..n {
                let v = (self.p[i * n + j] - k * self.pi[j].conj()) * inv_l;
                self.p[i * n + j] = v;
                self.p[j * n + i] = v.conj();
            }
        }
        // A runaway matrix (a stretch with no signal) starts over.
        if self.p[0].re.is_nan() || self.p[0].re.abs() >= 1e8 {
            self.reset_p();
        }
    }
}

fn psk_point(k: usize, m: usize) -> Complex64 {
    Complex64::from_polar(1.0, 2.0 * PI * k as f64 / m as f64)
}

fn slice(y: Complex64, m: usize) -> Complex64 {
    let k = (y.arg() / (2.0 * PI / m as f64))
        .round()
        .rem_euclid(m as f64) as usize;
    psk_point(k % m, m)
}

/// What frequency error is left after the coarse estimate, Hz: the slope of
/// the phase of the known preamble, correlated in 24 symbol pieces at the
/// timing reference, unwrapped and fitted by weighted least squares. The
/// coarse estimate is good to a few Hz, a fraction of a radian a piece, so
/// unwrapping is safe; the fit's 0.3 s baseline makes it good to a small
/// fraction of a hertz even near the decoding threshold.
pub fn fine_freq(x: &[Complex32], mode_idx: usize, tables: &Tables) -> f64 {
    let mut pts: Vec<(f64, f64, f64)> = Vec::new();
    let mut last = 0.0f64;
    for s in 0..PREAMBLE_LEN / FINE_SEG {
        let mut c = Complex64::new(0.0, 0.0);
        for k in s * FINE_SEG..(s + 1) * FINE_SEG {
            let v = x.get(2 * LEAD_SYM + 2 * k).copied().unwrap_or_default();
            let a = tables.known(mode_idx, k).unwrap_or(0.0) as f64;
            c += Complex64::new(v.re as f64, v.im as f64) * a;
        }
        let mut ph = c.arg();
        if !pts.is_empty() {
            ph = last + (ph - last + PI).rem_euclid(2.0 * PI) - PI;
        }
        last = ph;
        let t = (s as f64 + 0.5) * FINE_SEG as f64;
        pts.push((t, ph, c.norm_sqr()));
    }
    let sw: f64 = pts.iter().map(|p| p.2).sum();
    if pts.len() < 3 || sw <= 0.0 {
        return 0.0;
    }
    let mt = pts.iter().map(|p| p.0 * p.2).sum::<f64>() / sw;
    let mp = pts.iter().map(|p| p.1 * p.2).sum::<f64>() / sw;
    let num: f64 = pts.iter().map(|p| p.2 * (p.0 - mt) * (p.1 - mp)).sum();
    let den: f64 = pts.iter().map(|p| p.2 * (p.0 - mt) * (p.0 - mt)).sum();
    if den <= 0.0 {
        return 0.0;
    }
    num / den * SYMBOL_RATE / (2.0 * PI)
}

/// Channel taps either side of the timing reference for the SNR fit:
/// paths up to 4 symbols either side, with their pulses' tails.
const FIT_BEFORE: usize = 6;
const FIT_AFTER: usize = 6;
const FIT_TAPS: usize = FIT_BEFORE + FIT_AFTER + 1;
/// The fit runs separately over each A and over M1, 70 ms apiece, short
/// enough that a fade at 1 Hz spread changes the channel only a little
/// within one.
const FIT_WINDOWS: [(usize, usize); 3] = [(FIT_AFTER, 127), (127, 254), (254, 381 - FIT_BEFORE)];

/// Least squares fit of the channel's impulse response (taps a symbol
/// apart) to the known symbols `k_lo..k_hi` at the timing reference: the
/// taps' total power and the residual's, per symbol.
fn fit_window(
    x: &[Complex32],
    mode_idx: usize,
    tables: &Tables,
    k_lo: usize,
    k_hi: usize,
) -> Option<(f64, f64)> {
    const NT: usize = FIT_TAPS;
    let row = |k: usize| {
        let v = x.get(2 * LEAD_SYM + 2 * k).copied().unwrap_or_default();
        let v = Complex64::new(v.re as f64, v.im as f64);
        // Symbol k - i reaches the reference of symbol k through tap i.
        let a: [f64; NT] = std::array::from_fn(|t| {
            let i = t as i64 - FIT_BEFORE as i64;
            tables
                .known(mode_idx, (k as i64 - i) as usize)
                .unwrap_or(0.0) as f64
        });
        (v, a)
    };
    let mut r = [[0f64; NT]; NT];
    let mut h = [Complex64::new(0.0, 0.0); NT];
    for k in k_lo..k_hi {
        let (v, a) = row(k);
        for i in 0..NT {
            for j in 0..NT {
                r[i][j] += a[i] * a[j];
            }
            h[i] += v * a[i];
        }
    }
    // Gaussian elimination with partial pivoting.
    for c in 0..NT {
        let p = (c..NT).max_by(|&i, &j| r[i][c].abs().total_cmp(&r[j][c].abs()))?;
        if r[p][c].abs() < 1e-9 {
            return None;
        }
        r.swap(c, p);
        h.swap(c, p);
        let (top, rest) = r.split_at_mut(c + 1);
        let pivot = &top[c];
        for (k, row) in rest.iter_mut().enumerate() {
            let f = row[c] / pivot[c];
            for (x, p) in row[c..].iter_mut().zip(pivot[c..].iter()) {
                *x -= f * p;
            }
            let hc = h[c];
            h[c + 1 + k] -= hc * f;
        }
    }
    for c in (0..NT).rev() {
        let mut v = h[c];
        for j in c + 1..NT {
            v -= h[j] * r[c][j];
        }
        h[c] = v / r[c][c];
    }
    let mut res = 0f64;
    for k in k_lo..k_hi {
        let (v, a) = row(k);
        let model: Complex64 = h.iter().zip(a.iter()).map(|(h, a)| h * a).sum();
        res += (v - model).norm_sqr();
    }
    let dof = (k_hi - k_lo).saturating_sub(NT).max(1);
    Some((h.iter().map(|h| h.norm_sqr()).sum(), res / dof as f64))
}

/// Signal to noise in 3 kHz, dB, from the channel fits over the preamble.
/// The matched filter has unit energy, so a fit's residual power is N0 and
/// its taps' total power the symbol energy over all paths. What fading
/// changes within a fit counts as noise, so in fast fading this reads a
/// little low at high SNR. `None` if a fit is singular.
pub fn channel_snr(x: &[Complex32], mode_idx: usize, tables: &Tables) -> Option<f32> {
    let mut es = 0.0;
    let mut n0 = 0.0;
    for (lo, hi) in FIT_WINDOWS {
        let (e, n) = fit_window(x, mode_idx, tables, lo, hi)?;
        es += e;
        n0 += n;
    }
    let esn0 = es / n0.max(1e-12);
    Some((10.0 * esn0.log10() + 10.0 * (SYMBOL_RATE / 3000.0).log10()) as f32)
}

/// How the equalizer did on one probe: its gain on the known symbols and
/// the noise around that, so the soft values can be taken unbiased (a
/// least squares equalizer shrinks its output as the noise grows).
#[derive(Clone, Copy)]
struct ProbeFit {
    gain: f64,
    var: f64,
}

/// Demodulate and decode one burst. `x` holds samples half a symbol apart,
/// frequency corrected and normalized, with `x[2 * LEAD_SYM]` on the first
/// A symbol. `feedback` chooses the decision feedback shape.
pub fn demod(x: &[Complex32], mode_idx: usize, tables: &Tables, feedback: bool) -> Demod {
    let mode = MODES[mode_idx];
    let total = mode.burst_symbols();
    let bps = mode.bits_per_symbol;
    let m = 1usize << bps;
    let nfb = if feedback { NFB } else { 0 };
    let n = NFF + nfb;
    let mut rls = Rls::new(n);
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut uniform = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5
    };
    // Uniform deviates, scaled to the dither's variance (1/12 each).
    let amp = (DITHER / 2.0 * 12.0).sqrt();
    let xd: Vec<Complex64> = x
        .iter()
        .map(|s| Complex64::new(s.re as f64 + amp * uniform(), s.im as f64 + amp * uniform()))
        .collect();
    let mut decisions: Vec<Complex64> = Vec::with_capacity(total);
    let mut data_out: Vec<Complex64> = Vec::with_capacity(mode.data_symbols());
    // Each probe's fit (the preamble's last T first).
    let mut probes: Vec<ProbeFit> = Vec::with_capacity(mode.frames() + 1);
    let mut probe_y: Vec<(Complex64, Complex64)> = Vec::with_capacity(T_LEN);
    let mut phase = 0.0f64;
    let mut nu = 0.0f64;
    let mut u = vec![Complex64::new(0.0, 0.0); n];
    let mut u_w = vec![Complex64::new(0.0, 0.0); n];
    let last_t = PREAMBLE_LEN - T_LEN;
    for k in 0..total {
        let r = 2 * LEAD_SYM + 2 * k;
        let rot = Complex64::from_polar(1.0, -phase);
        for (i, slot) in u.iter_mut().take(NFF).enumerate() {
            *slot = xd.get(r + i - FF_LEFT).copied().unwrap_or_default() * rot;
        }
        for i in 0..nfb {
            u[NFF + i] = if k > i {
                decisions[k - 1 - i]
            } else {
                Complex64::new(0.0, 0.0)
            };
        }
        let y = rls.output(&u);
        let known = tables.known(mode_idx, k);
        let d = match known {
            Some(s) => Complex64::new(s as f64, 0.0),
            None => slice(y, m),
        };
        if known.is_some() {
            if k >= last_t {
                probe_y.push((y, d));
                if probe_y.len() == T_LEN {
                    let gain =
                        probe_y.iter().map(|(y, d)| (y * d.conj()).re).sum::<f64>() / T_LEN as f64;
                    let g = gain.max(0.05);
                    let var = probe_y
                        .iter()
                        .map(|(y, d)| (y - d * g).norm_sqr())
                        .sum::<f64>()
                        / T_LEN as f64;
                    probes.push(ProbeFit {
                        gain: g,
                        var: var.max(1e-4),
                    });
                    probe_y.clear();
                }
            }
        } else {
            data_out.push(y);
        }
        if known.is_some() {
            rls.update(&u, d - y);
        } else {
            // A weighted update: the regression row and its error scaled by
            // the weight's square root.
            let s = DD_WEIGHT.sqrt();
            for (us, v) in u_w.iter_mut().zip(u.iter()) {
                *us = v * s;
            }
            rls.update(&u_w, (d - y) * s);
        }
        let pe = (y * d.conj()).arg().clamp(-0.6, 0.6);
        nu += PLL_BETA * pe;
        phase += nu + PLL_ALPHA * pe;
        decisions.push(d);
    }

    // Soft values, chip by chip in the order sent.
    let mut chips = Vec::with_capacity(mode.chips());
    let points: Vec<(Complex64, u8)> = (0..m)
        .map(|k| (psk_point(k, m), index_to_gray(k as u8)))
        .collect();
    let mut dist = vec![0f64; m];
    for (i, y) in data_out.iter().enumerate() {
        // The probes either side of this data block.
        let frame = i / DATA_LEN;
        let (a, b) = (probes[frame], probes[(frame + 1).min(probes.len() - 1)]);
        let g = 0.5 * (a.gain + b.gain);
        let inv = 1.0 / (0.5 * (a.var + b.var));
        let z = if tables.scramble[i % SCRAMBLE_LEN] == 1 {
            -*y
        } else {
            *y
        };
        for (dk, (p, _)) in dist.iter_mut().zip(points.iter()) {
            *dk = (z - p * g).norm_sqr();
        }
        for bit in (0..bps).rev() {
            let (mut d0, mut d1) = (f64::MAX, f64::MAX);
            for (dk, (_, label)) in dist.iter().zip(points.iter()) {
                if label >> bit & 1 == 0 {
                    d0 = d0.min(*dk);
                } else {
                    d1 = d1.min(*dk);
                }
            }
            chips.push((((d1 - d0) * inv) as f32).clamp(-LLR_MAX, LLR_MAX));
        }
    }
    let map = interleave_map(&mode);
    let mut coded = vec![0f32; chips.len()];
    for (j, &c) in map.iter().enumerate() {
        coded[c as usize] = chips[j];
    }
    let llr: Vec<f32> = if mode.repeat == 2 {
        coded.chunks_exact(2).map(|p| p[0] + p[1]).collect()
    } else {
        coded
    };
    let bits = viterbi(&llr);
    // The probes' unbiased SNR, averaged as power so a fade counts as the
    // dip it is.
    let snr_lin =
        probes.iter().map(|p| p.gain * p.gain / p.var).sum::<f64>() / probes.len().max(1) as f64;
    Demod {
        bytes: pack_lsb_first(&bits),
        snr_db: (10.0 * snr_lin.max(1e-3).log10() + 10.0 * (SYMBOL_RATE / 3000.0).log10()) as f32,
        residual_hz: (nu * SYMBOL_RATE / (2.0 * PI)) as f32,
    }
}
