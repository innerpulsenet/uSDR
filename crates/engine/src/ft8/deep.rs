//! The second pass, for candidates the waterfall's soft bits could not
//! decode: each is mixed to baseband at 32 samples a symbol, aligned to a
//! sample and a sixteenth of a tone by its syncs, and its symbols measured
//! with a filter matched to exactly one symbol.
//!
//! Because the tones are spaced at the symbol rate and the phase runs on
//! continuously, a transmission keeps one phase from symbol to symbol in
//! this frame. So beside one symbol at a time, runs of two and three are
//! scored coherently, every tone combination tried, which is worth a dB or
//! two at the threshold. Each set of soft bits gets its own LDPC attempt.

use super::Protocol;
use super::demod::{FT4_COSTAS, FT4_GRAY, FT4_XOR, FT8_COSTAS, FT8_GRAY};
use super::ldpc::{self, N};
use rustfft::{FftPlanner, num_complex::Complex32};

/// Baseband samples per symbol.
const SPS: usize = 32;

/// The slot's spectrum, from which each candidate's baseband is cut.
pub struct Baseband {
    protocol: Protocol,
    spectrum: Vec<Complex32>,
    /// Hz per spectrum bin, and the baseband rate.
    df: f32,
    fs: f32,
    nbb: usize,
    ifft: std::sync::Arc<dyn rustfft::Fft<f32>>,
}

impl Baseband {
    pub fn new(protocol: Protocol, audio: &[f32], rate: f32) -> Baseband {
        let block = (rate * protocol.symbol_period()).round() as usize;
        let decim = block / SPS;
        // Zero padding past the slot keeps the circular transform from
        // wrapping a late signal onto the start; a multiple of the
        // decimation makes the baseband length whole.
        let mut nbb = (audio.len() + rate as usize).div_ceil(decim);
        // A length with only small prime factors transforms quickly.
        let smooth = |mut v: usize| {
            for f in [2, 3, 5] {
                while v.is_multiple_of(f) {
                    v /= f;
                }
            }
            v == 1
        };
        while !smooth(nbb) {
            nbb += 1;
        }
        let n = nbb * decim;
        let mut planner = FftPlanner::<f32>::new();
        let mut spectrum: Vec<Complex32> = (0..n).map(|i| Complex32::new(audio.get(i).copied().unwrap_or(0.0), 0.0)).collect();
        planner.plan_fft_forward(n).process(&mut spectrum);
        Baseband { protocol, spectrum, df: rate / n as f32, fs: rate / decim as f32, nbb, ifft: planner.plan_fft_inverse(nbb) }
    }

    /// The band from 1.5 tones below `f0` to 1.5 above the top tone, shifted
    /// so `f0` is at 0 Hz, edges tapered, at the baseband rate.
    fn extract(&self, f0: f32) -> Vec<Complex32> {
        let baud = 1.0 / self.protocol.symbol_period();
        let tones = self.protocol.tones() as f32;
        let lo = f0 - 1.5 * baud;
        let hi = f0 + (tones + 0.5) * baud;
        let i0 = (lo / self.df).round().max(0.0) as usize;
        let i1 = ((hi / self.df).round() as usize).min(self.spectrum.len() / 2);
        let ic = (f0 / self.df).round() as isize;
        let taper = ((0.5 * baud / self.df) as usize).max(1);
        let mut out = vec![Complex32::default(); self.nbb];
        let width = i1.saturating_sub(i0);
        for (k, i) in (i0..i1).enumerate() {
            let w = if k < taper {
                0.5 - 0.5 * (std::f32::consts::PI * k as f32 / taper as f32).cos()
            } else if width - k <= taper {
                0.5 - 0.5 * (std::f32::consts::PI * (width - k) as f32 / taper as f32).cos()
            } else {
                1.0
            };
            let j = (i as isize - ic).rem_euclid(self.nbb as isize) as usize;
            out[j] = self.spectrum[i] * w;
        }
        self.ifft.process(&mut out);
        out
    }
}

/// The baseband with `delta` tones of frequency error taken out, in
/// absolute time, so the phase it leaves is the same for every symbol.
fn rotate(x: &[Complex32], delta: f32, from: usize, to: usize) -> Vec<Complex32> {
    let mut out = vec![Complex32::default(); x.len()];
    let w = -std::f64::consts::TAU * delta as f64 / SPS as f64;
    let (to, from) = (to.min(x.len()), from.min(x.len()));
    // A phasor stepped sample by sample, restarted exactly every symbol so
    // rounding never builds up.
    let mut n = from;
    while n < to {
        let a = w * n as f64;
        let mut ph = num_complex::Complex64::new(a.cos(), a.sin());
        let step = num_complex::Complex64::new(w.cos(), w.sin());
        for m in n..(n + SPS).min(to) {
            out[m] = x[m] * Complex32::new(ph.re as f32, ph.im as f32);
            ph *= step;
        }
        n += SPS;
    }
    out
}

/// Symbol `k` of a transmission starting at baseband sample `n0`,
/// correlated with tone `t` over exactly one symbol.
fn tone_at(x: &[Complex32], n0: isize, k: usize, t: usize, rot: &[Vec<Complex32>]) -> Complex32 {
    let start = n0 + (k * SPS) as isize;
    let mut acc = Complex32::default();
    for (m, r) in rot[t].iter().enumerate() {
        let n = start + m as isize;
        if n >= 0
            && let Some(v) = x.get(n as usize)
        {
            acc += v * r;
        }
    }
    acc
}

/// A transmission aligned to a sample and a sixteenth of a tone: every
/// symbol measured at every tone, and where it was found.
struct Aligned {
    cs: Vec<[Complex32; 8]>,
    freq: f32,
    start: f32,
}

/// Align on the syncs near (`f0`, `start_s`) and measure every symbol.
fn align(bb: &Baseband, f0: f32, start_s: f32) -> Aligned {
    let p = bb.protocol;
    let tones = p.tones();
    let x = bb.extract(f0);
    let rot: Vec<Vec<Complex32>> = (0..tones)
        .map(|t| {
            (0..SPS)
                .map(|m| {
                    let a = -std::f32::consts::TAU * (t * m) as f32 / SPS as f32;
                    Complex32::new(a.cos(), a.sin())
                })
                .collect()
        })
        .collect();
    let syncs: Vec<(usize, usize)> = match p {
        Protocol::Ft8 => [0usize, 36, 72].iter().flat_map(|&g| (0..7).map(move |k| (g + k, FT8_COSTAS[k] as usize))).collect(),
        Protocol::Ft4 => [1usize, 34, 67, 100].iter().enumerate().flat_map(|(m, &g)| (0..4).map(move |k| (g + k, FT4_COSTAS[m][k] as usize))).collect(),
    };
    // Three eighths of a symbol either way, half a tone either way, first
    // on a coarse grid and then around its best.
    let n_center = (start_s * bb.fs).round() as isize;
    let span = (p.symbols() * SPS) as isize;
    let from = (n_center - 16).max(0) as usize;
    let to = (n_center + span + 16).max(0) as usize;
    let score = |xr: &[Complex32], n: isize| -> f32 { syncs.iter().map(|&(k, t)| tone_at(xr, n, k, t, &rot).norm_sqr()).sum() };
    let mut best = (f32::MIN, n_center, 0.0f32);
    for step in (-8..=8).step_by(2) {
        let delta = step as f32 / 16.0;
        let xr = rotate(&x, delta, from, to);
        for dn in (-12..=12isize).step_by(2) {
            let s = score(&xr, n_center + dn);
            if s > best.0 {
                best = (s, n_center + dn, delta);
            }
        }
    }
    let (_, n1, d1) = best;
    for step in -1..=1 {
        let delta = d1 + step as f32 / 16.0;
        let xr = rotate(&x, delta, from, to);
        for dn in -1..=1isize {
            let s = score(&xr, n1 + dn);
            if s > best.0 {
                best = (s, n1 + dn, delta);
            }
        }
    }
    let (_, n0, delta) = best;
    let xr = rotate(&x, delta, from, to);
    let cs = (0..p.symbols())
        .map(|k| {
            let mut out = [Complex32::default(); 8];
            for (t, o) in out.iter_mut().enumerate().take(tones) {
                *o = tone_at(&xr, n0, k, t, &rot);
            }
            out
        })
        .collect();
    Aligned { cs, freq: f0 + delta / p.symbol_period(), start: n0 as f32 / bb.fs }
}

/// Where a decoded transmission is and how strong: SNR in 2500 Hz, its
/// lowest tone's frequency, and its start in the slot.
///
/// With the matched filter on exactly one symbol, a tone two or more away
/// from the one sent holds only noise, so signal and noise are measured
/// from the same symbols.
pub fn measure(bb: &Baseband, f0: f32, start_s: f32, sent: &[u8]) -> (f32, f32, f32) {
    let p = bb.protocol;
    let a = align(bb, f0, start_s);
    let (mut sig, mut noise, mut nn) = (0.0f64, 0.0f64, 0);
    for (cs, &t) in a.cs.iter().zip(sent) {
        sig += cs[t as usize].norm_sqr() as f64;
        for (u, c) in cs.iter().enumerate().take(p.tones()) {
            if (u as i32 - t as i32).abs() >= 2 {
                noise += c.norm_sqr() as f64;
                nn += 1;
            }
        }
    }
    let noise = noise / nn.max(1) as f64;
    let sig = sig / sent.len().max(1) as f64;
    // The noise in one symbol's matched filter spans 1/T Hz.
    let per_symbol = ((sig - noise) / noise.max(1e-30)).max(1e-3);
    let snr = 10.0 * per_symbol.log10() - 10.0 * (2500.0 * p.symbol_period() as f64).log10();
    (snr as f32, a.freq, a.start)
}

/// Try one candidate near (`f0`, `start_s`). Returns the payload with the
/// refined frequency and start.
pub fn decode(bb: &Baseband, f0: f32, start_s: f32, iters: usize) -> Option<([u8; 10], f32, f32)> {
    let p = bb.protocol;
    let Aligned { mut cs, freq, start } = align(bb, f0, start_s);
    // Symbols a crash struck, brought down as in the first pass
    // ([`super::demod::loud_symbol_gains`]).
    let rms: Vec<f32> = cs.iter().map(|row| (row.iter().take(p.tones()).map(|c| c.norm_sqr()).sum::<f32>() / p.tones() as f32).sqrt()).collect();
    for (row, g) in cs.iter_mut().zip(super::demod::loud_symbol_gains(&rms)) {
        row.iter_mut().for_each(|c| *c *= g);
    }
    for nsym in 1..=3 {
        let llr = metrics(p, &cs, nsym);
        let (cw, errors) = ldpc::bp_decode(&llr, iters);
        if errors > 0 {
            continue;
        }
        let a91 = ldpc::pack91(&cw);
        if !ldpc::crc_ok(&a91) {
            continue;
        }
        let mut payload = [0u8; 10];
        payload.copy_from_slice(&a91[..10]);
        payload[9] &= 0xf8;
        if p == Protocol::Ft4 {
            for (b, x) in payload.iter_mut().zip(FT4_XOR) {
                *b ^= x;
            }
        }
        return Some((payload, freq, start));
    }
    None
}

/// Soft bits from runs of `nsym` symbols scored coherently: for each bit,
/// the best-matching run with that bit one against the best with it zero.
fn metrics(p: Protocol, cs: &[[Complex32; 8]], nsym: usize) -> [f32; N] {
    let tones = p.tones();
    let bps = tones.trailing_zeros() as usize;
    let gray: &[u8] = match p {
        Protocol::Ft8 => &FT8_GRAY,
        Protocol::Ft4 => &FT4_GRAY,
    };
    // Each run of 29 data symbols, by where it starts in the transmission.
    let segments: &[usize] = match p {
        Protocol::Ft8 => &[7, 43],
        Protocol::Ft4 => &[5, 38, 71],
    };
    let mut llr = [0f32; N];
    for (seg, &first) in segments.iter().enumerate() {
        let mut k = 0;
        while k < 29 {
            let run = nsym.min(29 - k);
            let nt = tones.pow(run as u32);
            let nbits = bps * run;
            let mut s2 = vec![0f32; nt];
            for (h, s) in s2.iter_mut().enumerate() {
                let mut sum = Complex32::default();
                for i in 0..run {
                    let d = (h / tones.pow((run - 1 - i) as u32)) % tones;
                    sum += cs[first + k + i][gray[d] as usize];
                }
                *s = sum.norm();
            }
            let bit0 = bps * (seg * 29 + k);
            for b in 0..nbits {
                let mask = 1 << (nbits - 1 - b);
                let (mut one, mut zero) = (0f32, 0f32);
                for (h, &v) in s2.iter().enumerate() {
                    if h & mask != 0 { one = one.max(v) } else { zero = zero.max(v) }
                }
                if bit0 + b < N {
                    llr[bit0 + b] = one - zero;
                }
            }
            k += run;
        }
    }
    let (sum, sum2) = llr.iter().fold((0.0f32, 0.0f32), |(a, b), v| (a + v, b + v * v));
    let var = (sum2 - sum * sum / N as f32) / N as f32;
    if var > 0.0 {
        let k = (24.0 / var).sqrt();
        llr.iter_mut().for_each(|v| *v *= k);
    }
    llr
}
