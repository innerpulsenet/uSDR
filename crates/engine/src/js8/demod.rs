//! From audio to frames: a waterfall of the slot, a search for the Costas
//! syncs in it, soft bits from the tones between them, and a finer second
//! pass for the candidates those could not decode.
//!
//! The shape is the FT8 decoder's (see `ft8::demod` and `ft8::deep`), with
//! JS8's timing, sync arrays and bit mapping. The waterfall is oversampled
//! twice in time and twice in frequency, so a signal starting between
//! symbol boundaries or sitting between tone bins still finds a view of
//! itself that lines up within a quarter of either.
//!
//! The second pass mixes each leftover candidate to baseband, aligns it to
//! a fraction of a symbol and a sixteenth of a tone on its syncs, and
//! measures each symbol with a filter matched to exactly one symbol. JS8 is
//! plain continuous-phase FSK with its tones spaced at the symbol rate, so
//! in that frame a transmission keeps one phase from symbol to symbol, and
//! runs of two and three symbols can be scored coherently.

use super::code::{self, N};
use super::{Js8Frame, Js8Speed, RATE_HZ, varicode};
use rustfft::{FftPlanner, num_complex::Complex32};

const TIME_OSR: usize = 2;
const FREQ_OSR: usize = 2;
const SYMBOLS: usize = 79;
const SYNC_AT: [usize; 3] = [0, 36, 72];
/// Sync candidates tried per slot, and the weakest sync worth trying, in
/// half-dB of contrast against the neighboring bins.
const MAX_CANDIDATES: usize = 300;
const MIN_SCORE: f32 = 5.0;
const LDPC_ITERATIONS: usize = 50;
/// Candidates the second pass may try, strongest first.
const DEEP_CANDIDATES: usize = 150;
/// Codeword bits the soft decisions may have had wrong in a frame worth
/// reporting. A 12-bit CRC passes one random word in 4096; this keeps the
/// rare word that belief propagation steers noise into from reaching the
/// CRC at all. Real frames at the threshold have a few dozen.
const MAX_HARD_ERRORS: u32 = 50;

/// Where in the transmission data symbol `k` sits, past the syncs.
fn data_symbol(k: usize) -> usize {
    k + if k < 29 { 7 } else { 14 }
}

/// How early and how late, in seconds from the nominal start, a
/// transmission is looked for: clocks are set by hand, and a station's can
/// be a second or two off.
fn dt_range(speed: Js8Speed) -> (f32, f32) {
    match speed {
        Js8Speed::Normal => (-1.5, 2.5),
        Js8Speed::Fast => (-1.0, 1.5),
        Js8Speed::Turbo => (-0.6, 0.9),
        Js8Speed::Slow => (-2.0, 3.0),
    }
}

/// Channel tones for a codeword: the syncs and three bits a symbol.
pub fn tones(speed: Js8Speed, cw: &[u8; N]) -> [u8; SYMBOLS] {
    let mut out = [0u8; SYMBOLS];
    for (g, costas) in SYNC_AT.iter().zip(speed.costas()) {
        out[*g..g + 7].copy_from_slice(&costas);
    }
    for k in 0..58 {
        out[data_symbol(k)] = (cw[3 * k] << 2) | (cw[3 * k + 1] << 1) | cw[3 * k + 2];
    }
    out
}

/// Continuous-phase FSK for a tone sequence, unit amplitude, at `rate`,
/// the first and last eighth of a symbol ramped so the keying does not
/// splatter.
pub fn fsk(speed: Js8Speed, tones: &[u8], f0: f32, rate: f32) -> Vec<f32> {
    let sps = (rate * speed.symbol_period()).round() as usize;
    let n = tones.len() * sps;
    let spacing = rate / sps as f32;
    let mut phi = 0.0f64;
    let mut out = Vec::with_capacity(n);
    for &t in tones {
        let dphi = std::f64::consts::TAU * (f0 + t as f32 * spacing) as f64 / rate as f64;
        for _ in 0..sps {
            out.push(phi.sin() as f32);
            phi = (phi + dphi) % std::f64::consts::TAU;
        }
    }
    let ramp = sps / 8;
    for i in 0..ramp {
        let env = (1.0 - (std::f32::consts::TAU * i as f32 / (2 * ramp) as f32).cos()) / 2.0;
        out[i] *= env;
        out[n - 1 - i] *= env;
    }
    out
}

/// Spectral power over the slot, in dB, for every symbol-long step and its
/// half-step, and every tone bin and its half-bin.
struct Waterfall {
    speed: Js8Speed,
    num_blocks: usize,
    num_bins: usize,
    /// The first tone bin kept, in tone spacings from 0 Hz.
    min_bin: usize,
    /// Samples per symbol and per FFT.
    block: usize,
    nfft: usize,
    mag: Vec<f32>,
}

#[derive(Clone, Copy, Debug)]
struct Candidate {
    score: f32,
    time_offset: i32,
    freq_offset: usize,
    time_sub: usize,
    freq_sub: usize,
}

impl Waterfall {
    fn new(speed: Js8Speed, audio: &[f32], f_min: f32, f_max: f32) -> Waterfall {
        let block = speed.symbol_samples();
        let period = speed.symbol_period();
        let sub = block / TIME_OSR;
        let nfft = block * FREQ_OSR;
        let min_bin = (f_min * period) as usize;
        let max_bin = (f_max * period) as usize + 1;
        let num_bins = max_bin - min_bin;
        let num_blocks = audio.len() / block;
        let norm = 2.0 / nfft as f32;
        let window: Vec<f32> = (0..nfft).map(|i| norm * (std::f32::consts::PI * i as f32 / nfft as f32).sin().powi(2)).collect();
        let fft = FftPlanner::<f32>::new().plan_fft_forward(nfft);
        let mut buf = vec![Complex32::default(); nfft];
        let mut scratch = vec![Complex32::default(); fft.get_inplace_scratch_len()];
        let mut mag = vec![0f32; num_blocks * TIME_OSR * FREQ_OSR * num_bins];
        let mut at = 0;
        for k in 0..num_blocks {
            for ts in 0..TIME_OSR {
                let end = (k * block + (ts + 1) * sub) as isize;
                let start = end - nfft as isize;
                for (i, c) in buf.iter_mut().enumerate() {
                    let n = start + i as isize;
                    let x = if n >= 0 { audio.get(n as usize).copied().unwrap_or(0.0) } else { 0.0 };
                    *c = Complex32::new(x * window[i], 0.0);
                }
                fft.process_with_scratch(&mut buf, &mut scratch);
                for fs in 0..FREQ_OSR {
                    for bin in min_bin..max_bin {
                        mag[at] = 10.0 * (1e-12 + buf[bin * FREQ_OSR + fs].norm_sqr()).log10();
                        at += 1;
                    }
                }
            }
        }
        Waterfall { speed, num_blocks, num_bins, min_bin, block, nfft, mag }
    }

    #[inline]
    fn at(&self, block: usize, c: &Candidate, tone: usize) -> f32 {
        self.mag[((block * TIME_OSR + c.time_sub) * FREQ_OSR + c.freq_sub) * self.num_bins + c.freq_offset + tone]
    }

    /// Audio frequency of a candidate's lowest tone.
    fn freq_hz(&self, c: &Candidate) -> f32 {
        (self.min_bin as f32 + c.freq_offset as f32 + c.freq_sub as f32 / FREQ_OSR as f32) / self.speed.symbol_period()
    }

    /// Where a candidate's first symbol starts, in seconds from the slot's
    /// start: each FFT is centered half its length before the end of the
    /// step it belongs to, and a symbol lines up when its center does.
    fn start_s(&self, c: &Candidate) -> f32 {
        let sub = (self.block / TIME_OSR) as f32;
        let center = c.time_offset as f32 * self.block as f32 + (c.time_sub as f32 + 1.0) * sub - self.nfft as f32 / 2.0;
        (center - self.block as f32 / 2.0) / RATE_HZ
    }

    /// How strongly the sync tones stand out where a candidate expects
    /// them, each against its neighbors a tone either side and a symbol
    /// either side, averaged, in half-dB: level-independent.
    fn sync_score(&self, c: &Candidate) -> f32 {
        let costas = self.speed.costas();
        let mut score = 0.0;
        let mut n = 0;
        for (&g, row) in SYNC_AT.iter().zip(&costas) {
            for (k, &sm) in row.iter().enumerate() {
                let b = c.time_offset + (g + k) as i32;
                if b < 0 {
                    continue;
                }
                if b as usize >= self.num_blocks {
                    break;
                }
                let b = b as usize;
                let sm = sm as usize;
                let p = self.at(b, c, sm);
                if sm > 0 {
                    score += p - self.at(b, c, sm - 1);
                    n += 1;
                }
                if sm < 7 {
                    score += p - self.at(b, c, sm + 1);
                    n += 1;
                }
                if k > 0 && b > 0 {
                    score += p - self.at(b - 1, c, sm);
                    n += 1;
                }
                if k < 6 && b + 1 < self.num_blocks {
                    score += p - self.at(b + 1, c, sm);
                    n += 1;
                }
            }
        }
        if n == 0 { 0.0 } else { 2.0 * score / n as f32 }
    }

    /// The strongest `max` sync candidates scoring at least `min_score`,
    /// strongest first.
    fn candidates(&self, max: usize, min_score: f32) -> Vec<Candidate> {
        let (lo, hi) = dt_range(self.speed);
        let period = self.speed.symbol_period();
        let d = self.speed.start_delay_s();
        let t0 = ((d + lo) / period).floor() as i32;
        let t1 = ((d + hi) / period).ceil() as i32;
        let mut out = Vec::new();
        for time_sub in 0..TIME_OSR {
            for freq_sub in 0..FREQ_OSR {
                for time_offset in t0..=t1 {
                    for freq_offset in 0..self.num_bins.saturating_sub(7) {
                        let mut c = Candidate { score: 0.0, time_offset, freq_offset, time_sub, freq_sub };
                        c.score = self.sync_score(&c);
                        if c.score >= min_score {
                            out.push(c);
                        }
                    }
                }
            }
        }
        out.sort_unstable_by(|a, b| b.score.total_cmp(&a.score));
        out.truncate(max);
        out
    }

    /// Soft bits, log(P(1)/P(0)), from the tones' amplitudes: for each bit
    /// the strongest tone that would mean a one against the strongest that
    /// would mean a zero.
    fn llrs(&self, c: &Candidate) -> [f32; N] {
        let mut llr = [0f32; N];
        for k in 0..58 {
            let b = c.time_offset + data_symbol(k) as i32;
            if b < 0 || b as usize >= self.num_blocks {
                continue;
            }
            let m: [f32; 8] = std::array::from_fn(|t| 10f32.powf(self.at(b as usize, c, t) / 20.0));
            for i in 0..3 {
                let mask = 4 >> i;
                let (mut one, mut zero) = (0f32, 0f32);
                for (t, &v) in m.iter().enumerate() {
                    if t & mask != 0 { one = one.max(v) } else { zero = zero.max(v) }
                }
                llr[3 * k + i] = one - zero;
            }
        }
        normalize(&mut llr);
        llr
    }
}

/// Scaled to the spread the decoder was tuned for, as in FT8.
fn normalize(llr: &mut [f32; N]) {
    let (sum, sum2) = llr.iter().fold((0.0f32, 0.0f32), |(a, b), v| (a + v, b + v * v));
    let var = (sum2 - sum * sum / N as f32) / N as f32;
    if var > 0.0 {
        let k = (24.0 / var).sqrt();
        llr.iter_mut().for_each(|v| *v *= k);
    }
}

/// Bits of a decoded codeword the soft decisions had the other way.
fn hard_errors(llr: &[f32; N], cw: &[u8; N]) -> u32 {
    llr.iter().zip(cw).filter(|&(l, &b)| (*l > 0.0) != (b == 1)).count() as u32
}

/// The slot's spectrum, from which each candidate's baseband is cut.
struct Baseband {
    speed: Js8Speed,
    /// Baseband samples per symbol.
    sps: usize,
    spectrum: Vec<Complex32>,
    /// Hz per spectrum bin, and the baseband rate.
    df: f32,
    fs: f32,
    nbb: usize,
    ifft: std::sync::Arc<dyn rustfft::Fft<f32>>,
}

impl Baseband {
    fn new(speed: Js8Speed, audio: &[f32]) -> Baseband {
        // A whole number of baseband samples a symbol, enough for the
        // band 1.5 tones either side of the signal at the slower speeds.
        let sps = match speed {
            Js8Speed::Normal | Js8Speed::Slow => 32,
            Js8Speed::Fast | Js8Speed::Turbo => 20,
        };
        let decim = speed.symbol_samples() / sps;
        // Zero padding past the slot keeps the circular transform from
        // wrapping a late signal onto the start.
        let mut nbb = (audio.len() + RATE_HZ as usize).div_ceil(decim);
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
        Baseband { speed, sps, spectrum, df: RATE_HZ / n as f32, fs: RATE_HZ / decim as f32, nbb, ifft: planner.plan_fft_inverse(nbb) }
    }

    /// The band from 1.5 tones below `f0` to 1.5 above the top tone, shifted
    /// so `f0` is at 0 Hz, edges tapered, at the baseband rate.
    fn extract(&self, f0: f32) -> Vec<Complex32> {
        let baud = self.speed.tone_spacing_hz();
        let lo = f0 - 1.5 * baud;
        let hi = f0 + 8.5 * baud;
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
fn rotate(x: &[Complex32], sps: usize, delta: f32, from: usize, to: usize) -> Vec<Complex32> {
    let mut out = vec![Complex32::default(); x.len()];
    let w = -std::f64::consts::TAU * delta as f64 / sps as f64;
    let (to, from) = (to.min(x.len()), from.min(x.len()));
    // A phasor stepped sample by sample, restarted exactly every symbol so
    // rounding never builds up.
    let step = num_complex::Complex64::new(w.cos(), w.sin());
    let mut n = from;
    while n < to {
        let a = w * n as f64;
        let mut ph = num_complex::Complex64::new(a.cos(), a.sin());
        for m in n..(n + sps).min(to) {
            out[m] = x[m] * Complex32::new(ph.re as f32, ph.im as f32);
            ph *= step;
        }
        n += sps;
    }
    out
}

/// Symbol `k` of a transmission starting at baseband sample `n0`,
/// correlated with tone `t` over exactly one symbol.
fn tone_at(x: &[Complex32], n0: isize, sps: usize, k: usize, rot: &[Complex32]) -> Complex32 {
    let start = n0 + (k * sps) as isize;
    let mut acc = Complex32::default();
    for (m, r) in rot.iter().enumerate() {
        let n = start + m as isize;
        if n >= 0
            && let Some(v) = x.get(n as usize)
        {
            acc += v * r;
        }
    }
    acc
}

/// A transmission aligned on its syncs: every symbol measured at every
/// tone, and where it was found.
struct Aligned {
    cs: Vec<[Complex32; 8]>,
    freq: f32,
    start: f32,
}

/// Align on the syncs near (`f0`, `start_s`) and measure every symbol.
fn align(bb: &Baseband, f0: f32, start_s: f32) -> Aligned {
    let sps = bb.sps;
    let x = bb.extract(f0);
    let rot: Vec<Vec<Complex32>> = (0..8)
        .map(|t| {
            (0..sps)
                .map(|m| {
                    let a = -std::f32::consts::TAU * (t * m) as f32 / sps as f32;
                    Complex32::new(a.cos(), a.sin())
                })
                .collect()
        })
        .collect();
    let costas = bb.speed.costas();
    let syncs: Vec<(usize, usize)> = SYNC_AT.iter().enumerate().flat_map(|(m, &g)| (0..7).map(move |k| (g + k, costas[m][k] as usize))).collect();
    // Three eighths of a symbol either way, half a tone either way, first
    // on a coarse grid and then around its best.
    let n_center = (start_s * bb.fs).round() as isize;
    let reach = (3 * sps / 8) as isize;
    let coarse = (sps / 16).max(1) as isize;
    let span = (SYMBOLS * sps) as isize;
    let from = (n_center - reach - sps as isize).max(0) as usize;
    let to = (n_center + span + reach + sps as isize).max(0) as usize;
    let score = |xr: &[Complex32], n: isize| -> f32 { syncs.iter().map(|&(k, t)| tone_at(xr, n, sps, k, &rot[t]).norm_sqr()).sum() };
    let mut best = (f32::MIN, n_center, 0.0f32);
    for step in (-8..=8).step_by(2) {
        let delta = step as f32 / 16.0;
        let xr = rotate(&x, sps, delta, from, to);
        let mut dn = -reach;
        while dn <= reach {
            let s = score(&xr, n_center + dn);
            if s > best.0 {
                best = (s, n_center + dn, delta);
            }
            dn += coarse;
        }
    }
    let (_, n1, d1) = best;
    for step in -1..=1 {
        let delta = d1 + step as f32 / 16.0;
        let xr = rotate(&x, sps, delta, from, to);
        for dn in -coarse..=coarse {
            let s = score(&xr, n1 + dn);
            if s > best.0 {
                best = (s, n1 + dn, delta);
            }
        }
    }
    let (_, n0, delta) = best;
    let xr = rotate(&x, sps, delta, from, to);
    let cs = (0..SYMBOLS).map(|k| std::array::from_fn(|t| tone_at(&xr, n0, sps, k, &rot[t]))).collect();
    Aligned { cs, freq: f0 + delta * bb.speed.tone_spacing_hz(), start: n0 as f32 / bb.fs }
}

/// Soft bits from runs of `nsym` symbols scored coherently: for each bit,
/// the best-matching run with that bit one against the best with it zero.
fn metrics(cs: &[[Complex32; 8]], nsym: usize) -> [f32; N] {
    let mut llr = [0f32; N];
    for (seg, first) in [7usize, 43].into_iter().enumerate() {
        let mut k = 0;
        while k < 29 {
            let run = nsym.min(29 - k);
            let nt = 8usize.pow(run as u32);
            let nbits = 3 * run;
            let s2: Vec<f32> = (0..nt)
                .map(|h| {
                    let mut sum = Complex32::default();
                    for i in 0..run {
                        sum += cs[first + k + i][(h >> (3 * (run - 1 - i))) & 7];
                    }
                    sum.norm()
                })
                .collect();
            let bit0 = 3 * (seg * 29 + k);
            for b in 0..nbits {
                let mask = 1 << (nbits - 1 - b);
                let (mut one, mut zero) = (0f32, 0f32);
                for (h, &v) in s2.iter().enumerate() {
                    if h & mask != 0 { one = one.max(v) } else { zero = zero.max(v) }
                }
                llr[bit0 + b] = one - zero;
            }
            k += run;
        }
    }
    normalize(&mut llr);
    llr
}

/// SNR in 2500 Hz, the lowest tone's frequency and the start in the slot of
/// a decoded transmission. With the matched filter on exactly one symbol,
/// a tone two or more away from the one sent holds only noise, so signal
/// and noise are measured from the same symbols.
fn measure(bb: &Baseband, f0: f32, start_s: f32, sent: &[u8]) -> (f32, f32, f32) {
    let a = align(bb, f0, start_s);
    let (mut sig, mut noise, mut nn) = (0.0f64, 0.0f64, 0);
    for (cs, &t) in a.cs.iter().zip(sent) {
        sig += cs[t as usize].norm_sqr() as f64;
        for (u, c) in cs.iter().enumerate() {
            if (u as i32 - t as i32).abs() >= 2 {
                noise += c.norm_sqr() as f64;
                nn += 1;
            }
        }
    }
    let noise = noise / nn.max(1) as f64;
    let sig = sig / sent.len().max(1) as f64;
    let per_symbol = ((sig - noise) / noise.max(1e-30)).max(1e-3);
    let snr = 10.0 * per_symbol.log10() - 10.0 * (2500.0 * bb.speed.symbol_period() as f64).log10();
    (snr as f32, a.freq, a.start)
}

/// A decode in the making: payload, flags, the codeword's hard errors, and
/// the frequency and start it was found at.
struct Found {
    payload: [u8; 9],
    flags: u8,
    hard: u32,
    freq: f32,
    start: f32,
}

fn accept(llr: &[f32; N]) -> Option<([u8; 9], u8, u32)> {
    let (payload, flags, cw) = code::decode(llr, LDPC_ITERATIONS)?;
    let hard = hard_errors(llr, &cw);
    (hard <= MAX_HARD_ERRORS).then_some((payload, flags, hard))
}

/// Decode one slot of one speed: audio at [`RATE_HZ`] from the slot's start.
pub fn decode_slot(speed: Js8Speed, audio: &[f32], f_min: f32, f_max: f32) -> Vec<Js8Frame> {
    let f_max = f_max.min(super::F_LIMIT_HZ);
    let f_min = f_min.clamp(0.0, f_max - 100.0);
    // Brought to one level first: the waterfall's dB has an absolute floor,
    // and audio taken ahead of an AGC can sit far below it.
    let rms = (audio.iter().map(|x| x * x).sum::<f32>() / audio.len().max(1) as f32).sqrt();
    if !(rms > 0.0 && rms.is_finite()) {
        return Vec::new();
    }
    let level: Vec<f32> = audio.iter().map(|x| x * (0.1 / rms)).collect();
    let audio = &level[..];
    let wf = Waterfall::new(speed, audio, f_min, f_max);
    if wf.num_blocks < SYMBOLS / 2 {
        return Vec::new();
    }
    // The second pass may take a twenty-fifth of the slot: the next one
    // is already filling, and other speeds may be waiting their turn.
    let budget = std::time::Duration::from_secs_f64(speed.slot_secs() / 25.0);
    let started = std::time::Instant::now();
    let tone = speed.tone_spacing_hz();
    let period = speed.symbol_period();
    let mut found: Vec<Found> = Vec::new();
    let mut missed = Vec::new();
    // A strong signal scores at several neighboring offsets; once one of
    // them has decoded, the others would only repeat it.
    let near = |found: &[Found], f: f32, t: f32| found.iter().any(|d| (d.freq - f).abs() < 0.6 * tone && (d.start - t).abs() < 0.6 * period);
    for c in wf.candidates(MAX_CANDIDATES, MIN_SCORE) {
        let (f, t) = (wf.freq_hz(&c), wf.start_s(&c));
        if near(&found, f, t) {
            continue;
        }
        match accept(&wf.llrs(&c)) {
            Some((payload, flags, hard)) => {
                if !found.iter().any(|d| d.payload == payload && d.flags == flags) {
                    found.push(Found { payload, flags, hard, freq: f, start: t });
                }
            }
            None => missed.push(c),
        }
    }
    let mut bb = None;
    for c in missed.into_iter().take(DEEP_CANDIDATES) {
        if started.elapsed() > budget {
            break;
        }
        let (f, t) = (wf.freq_hz(&c), wf.start_s(&c));
        if near(&found, f, t) {
            continue;
        }
        let bb = bb.get_or_insert_with(|| Baseband::new(speed, audio));
        let a = align(bb, f, t);
        for nsym in 1..=3 {
            if let Some((payload, flags, hard)) = accept(&metrics(&a.cs, nsym)) {
                if !found.iter().any(|d| d.payload == payload && d.flags == flags) {
                    found.push(Found { payload, flags, hard, freq: a.freq, start: a.start });
                }
                break;
            }
        }
    }
    let bb = bb.unwrap_or_else(|| Baseband::new(speed, audio));
    found
        .into_iter()
        .map(|d| {
            let sent = tones(speed, &code::encode(&d.payload, d.flags));
            let (snr_db, freq_hz, start_s) = measure(&bb, d.freq, d.start, &sent);
            Js8Frame {
                speed,
                slot_ms: 0,
                snr_db: snr_db.clamp(-35.0, 50.0).round(),
                dt_s: start_s - speed.start_delay_s(),
                freq_hz,
                flags: d.flags,
                payload: d.payload,
                raw: varicode::raw_text(&d.payload),
                content: varicode::unpack(&d.payload, d.flags),
                hard_errors: d.hard,
            }
        })
        .collect()
}
