//! From baseband to code bits: a search of the slot for the sync pattern,
//! a finer fit of each candidate's frequency, start and drift, and soft
//! bits from the tones.
//!
//! WSPR sends one sync bit and one data bit in every symbol: the tone is
//! the sync bit plus twice the data bit. Knowing the sync pattern, each
//! symbol is a choice between two tones, and the other two hold only noise,
//! which is what the noise level is measured on.

use super::fano::{self, FanoParams, NSYM};
use super::{BASEBAND_HZ, NOMINAL_START, SPS};
use rustfft::{FftPlanner, num_complex::Complex32};
use std::f32::consts::TAU;

/// The sync bit of every channel symbol.
pub const SYNC: [u8; NSYM] = [
    1, 1, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 1, 1, 0, 0, 0, 1, 0, 0, 1, 0, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 1, 0, 1, 0, 0, 0,
    0, 0, 0, 1, 0, 1, 1, 0, 0, 1, 1, 0, 1, 0, 0, 0, 1, 1, 0, 1, 0, 0, 0, 0, 1, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 0, 1, 0, 0, 1, 0, 1,
    1, 0, 0, 0, 1, 1, 0, 1, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 1, 0, 0, 1, 0, 0, 1, 1, 1, 0, 1, 1, 0, 0, 1, 1, 0, 1, 0, 0, 0, 1, 1,
    1, 0, 0, 0, 0, 0, 1, 0, 1, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 1, 1, 0, 1, 0, 1, 1, 0, 0, 0, 1, 1, 0, 0, 0,
];

/// Samples a transmission spans.
pub const SPAN: usize = NSYM * SPS;
/// Tone spacing, which is also the symbol rate.
pub const DF: f32 = BASEBAND_HZ / SPS as f32;

/// Channel tones (0..3) for a 50-bit message.
pub fn tones(bits: u64) -> [u8; NSYM] {
    let code = fano::encode(bits);
    let map = fano::interleave_map();
    let mut sent = [0u8; NSYM];
    for (p, &j) in map.iter().enumerate() {
        sent[j] = code[p];
    }
    std::array::from_fn(|k| SYNC[k] + 2 * sent[k])
}

/// Where a signal is: the center of its four tones (baseband Hz, at the
/// middle of the transmission), its total drift across the transmission
/// (Hz), and the sample its first symbol starts at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fit {
    pub freq: f32,
    pub drift: f32,
    pub start: i32,
}

impl Fit {
    /// The center frequency during symbol `k`.
    fn freq_at(&self, k: usize) -> f32 {
        self.freq + self.drift * (k as f32 + 0.5 - NSYM as f32 / 2.0) / NSYM as f32
    }
}

/// Correlations of each symbol with each of the four tones.
pub struct Tones {
    pub c: Vec<[Complex32; 4]>,
}

impl Tones {
    pub fn power(&self, k: usize, j: usize) -> f32 {
        self.c[k][j].norm_sqr()
    }

    /// How strongly the tones follow the sync pattern: the sync-matching
    /// pair's power less the other pair's, over all of it.
    pub fn sync_metric(&self) -> f32 {
        let (mut num, mut den) = (0.0, 0.0);
        for (k, &sync) in SYNC.iter().enumerate() {
            let p: [f32; 4] = std::array::from_fn(|j| self.power(k, j));
            let d = (p[1] + p[3]) - (p[0] + p[2]);
            num += if sync == 1 { d } else { -d };
            den += p.iter().sum::<f32>();
        }
        if den > 0.0 { num / den } else { 0.0 }
    }
}

/// The four tone references across one symbol, at half-integer DFT bins.
pub struct Refs {
    w: Vec<[Complex32; 4]>,
}

impl Refs {
    pub fn new() -> Self {
        let w = (0..SPS)
            .map(|n| std::array::from_fn(|j| Complex32::from_polar(1.0, -TAU * (j as f32 - 1.5) * n as f32 / SPS as f32)))
            .collect();
        Refs { w }
    }
}

/// Correlate every symbol of `fit` with its four tones. Samples outside
/// `z` count as silence.
pub fn tone_correlations(z: &[Complex32], fit: &Fit, refs: &Refs) -> Tones {
    let mut c = vec![[Complex32::new(0.0, 0.0); 4]; NSYM];
    for (k, ck) in c.iter_mut().enumerate() {
        let base = fit.start as i64 + (k * SPS) as i64;
        let lo = (-base).clamp(0, SPS as i64) as usize;
        let hi = (z.len() as i64 - base).clamp(0, SPS as i64) as usize;
        if lo >= hi {
            continue;
        }
        let step = Complex32::from_polar(1.0, -TAU * fit.freq_at(k) / BASEBAND_HZ);
        let mut rot = Complex32::from_polar(1.0, -TAU * fit.freq_at(k) * lo as f32 / BASEBAND_HZ);
        let mut acc = [Complex32::new(0.0, 0.0); 4];
        for n in lo..hi {
            let y = z[(base + n as i64) as usize] * rot;
            rot *= step;
            let w = &refs.w[n];
            for j in 0..4 {
                acc[j] += y * w[j];
            }
        }
        *ck = acc;
    }
    Tones { c }
}

/// The slot's spectrogram, one symbol long and half a symbol apart, at
/// half-tone resolution, with the sync pattern's two sums precomputed.
pub struct Spectrogram {
    /// Bins kept either side of the center.
    pub half_bins: i32,
    frames: usize,
    /// Per frame and center bin: the odd tones' power less the even ones',
    /// and all four.
    diff: Vec<f32>,
    total: Vec<f32>,
}

/// Half a symbol: the spectrogram's hop.
const HOP: usize = SPS / 2;
const NFFT: usize = 2 * SPS;
/// Spectrogram bin width.
pub const BIN_HZ: f32 = BASEBAND_HZ / NFFT as f32;

impl Spectrogram {
    pub fn new(z: &[Complex32], max_hz: f32) -> Self {
        let half_bins = (max_hz / BIN_HZ).ceil() as i32 + 8;
        let frames = if z.len() >= SPS { (z.len() - SPS) / HOP + 1 } else { 0 };
        let fft = FftPlanner::<f32>::new().plan_fft_forward(NFFT);
        let width = (2 * half_bins + 1) as usize;
        let mut power = vec![0f32; frames * width];
        let mut buf = vec![Complex32::new(0.0, 0.0); NFFT];
        for f in 0..frames {
            buf.iter_mut().for_each(|b| *b = Complex32::new(0.0, 0.0));
            buf[..SPS].copy_from_slice(&z[f * HOP..f * HOP + SPS]);
            fft.process(&mut buf);
            for b in -half_bins..=half_bins {
                power[f * width + (b + half_bins) as usize] = buf[b.rem_euclid(NFFT as i32) as usize].norm_sqr();
            }
        }
        // Tones sit at the center -3, -1, +1 and +3 bins.
        let mut diff = vec![0f32; frames * width];
        let mut total = vec![0f32; frames * width];
        for f in 0..frames {
            let p = &power[f * width..(f + 1) * width];
            for c in 3..width - 3 {
                let (p0, p1, p2, p3) = (p[c - 3], p[c - 1], p[c + 1], p[c + 3]);
                diff[f * width + c] = (p1 + p3) - (p0 + p2);
                total[f * width + c] = p0 + p1 + p2 + p3;
            }
        }
        Spectrogram { half_bins, frames, diff, total }
    }

    fn width(&self) -> usize {
        (2 * self.half_bins + 1) as usize
    }

    /// The best sync over starts and drifts for every center bin within
    /// `max_hz`: (bin, score, drift Hz, start frame).
    pub fn search(&self, max_hz: f32, lags: std::ops::RangeInclusive<i32>, drifts: &[f32]) -> Vec<(i32, f32, f32, i32)> {
        let width = self.width() as i32;
        let reach = (max_hz / BIN_HZ).ceil() as i32;
        let mut out = Vec::new();
        // Each symbol's offset in bins for each drift.
        let shifts: Vec<Vec<i32>> = drifts
            .iter()
            .map(|d| (0..NSYM).map(|k| (d * (k as f32 + 0.5 - NSYM as f32 / 2.0) / NSYM as f32 / BIN_HZ).round() as i32).collect())
            .collect();
        for b in -reach..=reach {
            let c0 = b + self.half_bins;
            let mut best = (f32::MIN, 0.0, 0);
            for (di, sh) in shifts.iter().enumerate() {
                for lag in lags.clone() {
                    let (mut num, mut den) = (0.0f32, 0.0f32);
                    for k in 0..NSYM {
                        let f = lag + 2 * k as i32;
                        if f < 0 || f as usize >= self.frames {
                            continue;
                        }
                        let c = c0 + sh[k];
                        if !(3..width - 3).contains(&c) {
                            continue;
                        }
                        let i = f as usize * width as usize + c as usize;
                        num += if SYNC[k] == 1 { self.diff[i] } else { -self.diff[i] };
                        den += self.total[i];
                    }
                    let s = if den > 0.0 { num / den } else { 0.0 };
                    if s > best.0 {
                        best = (s, drifts[di], lag);
                    }
                }
            }
            out.push((b, best.0, best.1, best.2));
        }
        out
    }
}

/// Refine a candidate's start, frequency and drift by the sync metric
/// measured on the tones themselves.
pub fn refine(z: &[Complex32], mut fit: Fit, refs: &Refs) -> (Fit, f32) {
    let mut best = tone_correlations(z, &fit, refs).sync_metric();
    let try_fit = |fit: &mut Fit, best: &mut f32, cands: &mut dyn Iterator<Item = Fit>| {
        let mut chosen = *fit;
        for f in cands {
            let s = tone_correlations(z, &f, refs).sync_metric();
            if s > *best {
                *best = s;
                chosen = f;
            }
        }
        *fit = chosen;
    };
    let f0 = fit;
    try_fit(&mut fit, &mut best, &mut (-4..=4).map(|i| Fit { start: f0.start + 32 * i, ..f0 }));
    let f1 = fit;
    try_fit(&mut fit, &mut best, &mut (-4..=4).map(|i| Fit { freq: f1.freq + 0.1 * i as f32, ..f1 }));
    let f2 = fit;
    try_fit(&mut fit, &mut best, &mut (-2..=2).map(|i| Fit { drift: f2.drift + 0.25 * i as f32, ..f2 }));
    let f3 = fit;
    try_fit(&mut fit, &mut best, &mut (-3..=3).map(|i| Fit { start: f3.start + 8 * i, ..f3 }));
    let f4 = fit;
    try_fit(&mut fit, &mut best, &mut (-2..=2).map(|i| Fit { freq: f4.freq + 0.04 * i as f32, ..f4 }));
    (fit, best)
}

/// Power in the tones a decoded message says were sent.
fn sent_power(z: &[Complex32], fit: &Fit, sent: &[u8; NSYM]) -> f32 {
    let mut total = 0.0;
    for (k, &j) in sent.iter().enumerate() {
        let base = fit.start as i64 + (k * SPS) as i64;
        let lo = (-base).clamp(0, SPS as i64) as usize;
        let hi = (z.len() as i64 - base).clamp(0, SPS as i64) as usize;
        let f = fit.freq_at(k) + (j as f32 - 1.5) * DF;
        let step = Complex32::from_polar(1.0, -TAU * f / BASEBAND_HZ);
        let mut rot = Complex32::from_polar(1.0, -TAU * f * lo as f32 / BASEBAND_HZ);
        let mut acc = Complex32::new(0.0, 0.0);
        for n in lo..hi {
            acc += z[(base + n as i64) as usize] * rot;
            rot *= step;
        }
        total += acc.norm_sqr();
    }
    total
}

/// Refit a decoded signal on the tones it is now known to have sent: the
/// sync metric is blunt to drift (a hertz moves only the ends of the
/// transmission by a third of a tone), the sent tones' power is not.
pub fn fit_known(z: &[Complex32], mut fit: Fit, sent: &[u8; NSYM]) -> Fit {
    let mut best = sent_power(z, &fit, sent);
    for round in 0..2 {
        let scale = if round == 0 { 1.0 } else { 0.4 };
        let base = fit;
        let tries = (-4..=4)
            .map(|i| Fit { drift: base.drift + 0.25 * scale * i as f32, ..base })
            .chain((-4..=4).map(|i| Fit { freq: base.freq + 0.04 * scale * i as f32, ..base }))
            .chain((-4..=4).map(|i| Fit { start: base.start + (4.0 * scale * i as f32).round() as i32, ..base }));
        for f in tries {
            // One knob at a time, from the best so far.
            let f = Fit {
                drift: if f.drift != base.drift { f.drift } else { fit.drift },
                freq: if f.freq != base.freq { f.freq } else { fit.freq },
                start: if f.start != base.start { f.start } else { fit.start },
            };
            let p = sent_power(z, &f, sent);
            if p > best {
                best = p;
                fit = f;
            }
        }
    }
    fit
}

/// ln I0(x), the log of the modified Bessel function: how likely a tone of
/// a given amplitude is under a carrier, against noise alone.
fn ln_i0(x: f32) -> f32 {
    let x = x.abs();
    if x < 3.75 {
        let t = (x / 3.75).powi(2);
        (1.0 + t * (3.515_623 + t * (3.089_942_4 + t * (1.206_749_2 + t * (0.265_973_2 + t * (0.036_076_8 + t * 0.004_581_3))))))
            .ln()
    } else {
        let u = 3.75 / x;
        let p = 0.398_942_3
            + u * (0.013_285_92
                + u * (0.002_253_19
                    + u * (-0.001_575_65 + u * (0.009_162_81 + u * (-0.020_577_06 + u * (0.026_355_37 + u * (-0.016_476_33 + u * 0.003_923_77)))))));
        x - 0.5 * x.ln() + p.ln()
    }
}

/// Per-channel-symbol log-likelihood ratios (data bit 1 over 0), from the
/// two tones the sync bit leaves to choose between, scored as a carrier of
/// the measured amplitude in the measured noise.
///
/// With `block` above 1, consecutive symbols are scored together: the
/// transmitter's phase runs on unbroken from one tone to the next, and
/// every tone, whichever it is, ends a symbol half a cycle on from where
/// the center frequency would, so a run of symbols can be added up
/// coherently once that is taken out. Each block's 2^block tone sequences
/// are scored on the size of their sum, and each bit's ratio is taken over
/// the sequences that give it either value. Worth about a decibel where the
/// transmitter keeps its phase and the path does not stir it; worth nothing
/// where they do, which is why single symbols are tried first.
pub fn llrs(t: &Tones, fit: &Fit, block: usize) -> [f32; NSYM] {
    let mut noise = 0.0;
    let mut both = 0.0;
    for (k, &sync) in SYNC.iter().enumerate() {
        let s = sync as usize;
        noise += t.power(k, 1 - s) + t.power(k, 3 - s);
        both += t.power(k, s) + t.power(k, s + 2);
    }
    let n = (noise / (2 * NSYM) as f32).max(1e-30);
    let a2 = (both / NSYM as f32 - 2.0 * n).max(0.05 * n);
    // ln I0 of a coherent sum of `block` symbols: the amplitude adds up
    // `block` times and so does the noise power, so the gain on the sum's
    // size is the one-symbol gain.
    let gain = 2.0 * a2.sqrt() / n;
    if block <= 1 {
        return std::array::from_fn(|k| {
            let s = SYNC[k] as usize;
            ln_i0(gain * t.c[k][s + 2].norm()) - ln_i0(gain * t.c[k][s].norm())
        });
    }
    // Each symbol's correlations turned back by the phase the carrier ran
    // through before it.
    let mut phase = 0.0f32;
    let c: Vec<[Complex32; 4]> = (0..NSYM)
        .map(|k| {
            let r = Complex32::from_polar(1.0, -phase);
            phase = (phase + TAU * fit.freq_at(k) * SPS as f32 / BASEBAND_HZ + std::f32::consts::PI).rem_euclid(TAU);
            t.c[k].map(|v| v * r)
        })
        .collect();
    let mut out = [0f32; NSYM];
    let mut k0 = 0;
    while k0 < NSYM {
        let len = block.min(NSYM - k0);
        let scores: Vec<f32> = (0..1usize << len)
            .map(|h| {
                let mut sum = Complex32::new(0.0, 0.0);
                for i in 0..len {
                    let k = k0 + i;
                    sum += c[k][SYNC[k] as usize + 2 * ((h >> i) & 1)];
                }
                ln_i0(gain * sum.norm())
            })
            .collect();
        for i in 0..len {
            let lse = |bit: usize| {
                let of = || scores.iter().enumerate().filter(move |(h, _)| (h >> i) & 1 == bit).map(|(_, &s)| s);
                let m = of().fold(f32::MIN, f32::max);
                m + of().map(|s| (s - m).exp()).sum::<f32>().ln()
            };
            out[k0 + i] = lse(1) - lse(0);
        }
        k0 += len;
    }
    out
}

/// How well a codeword fits the received symbols: the sum over its code
/// bits of log2 of twice the probability the symbols give that bit, the
/// Fano metric without its bias. A message read out of noise fits about as
/// well as noise allows; a real one fits far better.
pub fn fit_score(llr: &[f32; NSYM], sent: &[u8; NSYM]) -> f32 {
    llr.iter()
        .zip(sent)
        .map(|(&l, &s)| {
            let l = if s >= 2 { l } else { -l };
            1.0 - (1.0 + (-l.clamp(-20.0, 20.0)).exp()).log2()
        })
        .sum()
}

/// Fano's per-bit metrics from LLRs in channel order.
pub fn bit_metrics(llr: &[f32; NSYM], bias: f32) -> [(i32, i32); NSYM] {
    let map = fano::interleave_map();
    std::array::from_fn(|p| {
        let l = llr[map[p]].clamp(-20.0, 20.0);
        // log2 of P(bit | r) / P(bit), less the code rate's share.
        let m = |l: f32| 1.0 - (1.0 + (-l).exp()).log2() - bias;
        ((m(-l) * fano::SCALE).round() as i32, (m(l) * fano::SCALE).round() as i32)
    })
}

/// Fano decode a fitted candidate, scoring `block` symbols at a time;
/// the decode and the LLRs it came from.
pub fn decode(t: &Tones, fit: &Fit, block: usize, bias: f32, params: FanoParams) -> Option<(u64, [f32; NSYM])> {
    let llr = llrs(t, fit, block);
    fano::fano(&bit_metrics(&llr, bias), params).map(|d| (d, llr))
}

/// Signal to noise in 2500 Hz from the tones a decoded message says were
/// sent: power in the sent tone over the mean of the other three, whose
/// bandwidth is one tone spacing.
pub fn snr_db(t: &Tones, sent: &[u8; NSYM]) -> f32 {
    let (mut sig, mut noise) = (0.0, 0.0);
    for (k, &s) in sent.iter().enumerate() {
        for j in 0..4 {
            if j == s as usize {
                sig += t.power(k, j);
            } else {
                noise += t.power(k, j);
            }
        }
    }
    let n = noise / (3 * NSYM) as f32;
    let s = sig / NSYM as f32 - n;
    if s <= 0.0 || n <= 0.0 {
        return -40.0;
    }
    10.0 * (s / n * DF / 2500.0).log10()
}

/// Take a decoded signal out of `z`: each symbol's sent tone, at the phase
/// and amplitude measured on it, so what was under it can be searched for.
pub fn subtract(z: &mut [Complex32], fit: &Fit, sent: &[u8; NSYM], t: &Tones) {
    for (k, &tone) in sent.iter().enumerate() {
        let j = tone as usize;
        let base = fit.start as i64 + (k * SPS) as i64;
        let f = fit.freq_at(k) + (j as f32 - 1.5) * DF;
        // The correlation was against a unit tone over SPS samples.
        let amp = t.c[k][j] / SPS as f32;
        let step = Complex32::from_polar(1.0, TAU * f / BASEBAND_HZ);
        let mut rot = Complex32::new(1.0, 0.0);
        for n in 0..SPS as i64 {
            let i = base + n;
            if i >= 0 && (i as usize) < z.len() {
                z[i as usize] -= amp * rot;
            }
            rot *= step;
        }
    }
}

/// Start of a transmission in seconds relative to the nominal 1 s into
/// the slot.
pub fn dt_s(start: i32) -> f32 {
    (start - NOMINAL_START as i32) as f32 / BASEBAND_HZ
}
