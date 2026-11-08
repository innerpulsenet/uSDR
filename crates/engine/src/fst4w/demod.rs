//! From baseband to soft bits: the candidate search, a per-candidate
//! downconversion, the sync search, and the bit metrics of
//! `get_fst4_bitmetrics.f90` (Joe Taylor K1JT, Steven Franke K9AN).

use super::{ldpc, Period, BASEBAND_HZ};
use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::f32::consts::TAU;
use std::sync::Arc;

/// Symbols in a frame.
pub const NN: usize = 160;
/// Samples per symbol of a candidate's downconverted signal.
pub const NSS: usize = 40;
const SYNC_LEN: usize = 8 * NSS;
pub const SYNC1: [u8; 8] = [0, 1, 3, 2, 1, 0, 2, 3];
pub const SYNC2: [u8; 8] = [2, 3, 1, 0, 3, 2, 0, 1];
/// Bits (two per symbol) to tone.
const GRAY: [usize; 4] = [0, 1, 3, 2];
/// Where each sync word begins, in symbols.
const SYNC_AT: [(usize, bool); 5] = [(0, false), (38, true), (76, false), (114, true), (152, false)];

/// Candidate frequencies are searched this far either side of center.
const SEARCH_HZ: f32 = 110.0;
/// The noise baseline is fit over this far either side of center.
const NOISE_HZ: f32 = 145.0;
/// Weakest spectral peak worth trying (ratio to the noise baseline, for the
/// 4-tone sum), as in the source. Noise alone rarely crosses it.
const MIN_SYNC: f32 = 1.2;
const MAX_CANDIDATES: usize = 60;
/// How the source's CLEAN removes a found peak from the spectrum.
const CLEAN: [f32; 7] = [0.25, 0.5, 0.75, 1.0, 0.75, 0.5, 0.25];
/// Passes of the hard-sync gate before metrics are computed, and the
/// bit-level sync quality before decoding is tried.
const MIN_HARD_SYNC: usize = 16;
const MIN_SYNC_QUAL: usize = 46;

/// The sync patterns and tone references, built once per slot.
pub struct Refs {
    tones: [[Complex32; NSS]; 4],
    sync1: [Complex32; SYNC_LEN],
    sync2: [Complex32; SYNC_LEN],
}

impl Refs {
    pub fn new() -> Self {
        let dphi = TAU / NSS as f32;
        let mut tones = [[Complex32::new(0.0, 0.0); NSS]; 4];
        for (t, row) in tones.iter_mut().enumerate() {
            for (j, v) in row.iter_mut().enumerate() {
                *v = Complex32::from_polar(1.0, (t as f32 - 1.5) * dphi * j as f32);
            }
        }
        let word = |w: &[u8; 8]| {
            let mut out = [Complex32::new(0.0, 0.0); SYNC_LEN];
            let mut phi = 0f32;
            for (i, &tone) in w.iter().enumerate() {
                let dp = dphi * (tone as f32 - 1.5);
                for s in &mut out[i * NSS..(i + 1) * NSS] {
                    *s = Complex32::from_polar(1.0, phi);
                    phi = (phi + dp).rem_euclid(TAU);
                }
            }
            out
        };
        Refs { tones, sync1: word(&SYNC1), sync2: word(&SYNC2) }
    }
}

impl Default for Refs {
    fn default() -> Self {
        Self::new()
    }
}

/// A candidate from the spectrum: its center frequency relative to the
/// baseband center, and the noise baseline there.
#[derive(Clone, Copy, Debug)]
pub struct Candidate {
    pub f: f32,
    pub base: f32,
}

/// A slot's spectrum, from which candidates are found and downconverted.
pub struct Baseband {
    pub period: Period,
    x: Vec<Complex32>,
    m: usize,
    l: usize,
    ifft: Arc<dyn Fft<f32>>,
    scratch: Vec<Complex32>,
}

impl Baseband {
    pub fn new(z: &[Complex32], period: Period) -> Self {
        let m = period.window_symbols();
        let l = period.window_samples();
        let mut x = vec![Complex32::new(0.0, 0.0); l];
        let n = z.len().min(l);
        x[..n].copy_from_slice(&z[..n]);
        let mut planner = FftPlanner::<f32>::new();
        planner.plan_fft_forward(l).process(&mut x);
        let ifft = planner.plan_fft_inverse(m * NSS);
        let scratch = vec![Complex32::new(0.0, 0.0); ifft.get_inplace_scratch_len()];
        Baseband { period, x, m, l, ifft, scratch }
    }

    fn bin(&self, k: i64) -> Complex32 {
        self.x[k.rem_euclid(self.l as i64) as usize]
    }

    /// `(frequency relative to center, noise baseline)`, strongest first.
    pub fn candidates(&self) -> Vec<Candidate> {
        let baud = self.period.baud();
        let df2 = baud / 2.0;
        let hw = (NOISE_HZ / df2).round() as usize;
        let n = 2 * hw + 1;
        let nd = self.m / 2;
        let ndh = (nd / 2) as i64;
        let mut s = vec![0f32; n];
        for (idx, v) in s.iter_mut().enumerate() {
            let i = idx as i64 - hw as i64;
            let j0 = (i * self.m as i64) / 2;
            for j in j0 - ndh..=j0 + ndh {
                *v += self.bin(j).norm_sqr();
            }
        }
        let mut s2 = vec![0f32; n];
        for i in 3..n - 3 {
            s2[i] = s[i - 3] + s[i - 1] + s[i + 1] + s[i + 3];
        }
        let sbase = baseline(&s2, 6, n - 7);
        for i in 3..n - 3 {
            if sbase[i] > 0.0 {
                s2[i] /= sbase[i];
            }
        }
        let ia = hw - (SEARCH_HZ / df2).round() as usize;
        let ib = hw + (SEARCH_HZ / df2).round() as usize;
        let mut out = Vec::new();
        while out.len() < MAX_CANDIDATES {
            let (iloc, &pval) = s2[ia..=ib].iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).expect("nonempty");
            let iloc = ia + iloc;
            if pval < MIN_SYNC {
                break;
            }
            for (w, &xdb) in CLEAN.iter().enumerate() {
                let k = iloc as i64 + 2 * (w as i64 - 3);
                if k >= ia as i64 && k <= ib as i64 {
                    let k = k as usize;
                    s2[k] = (s2[k] - 0.9 * pval * xdb).max(0.0);
                }
            }
            out.push(Candidate { f: df2 * (iloc as f32 - hw as f32), base: sbase[iloc] });
        }
        out
    }

    /// The signal around `f` (relative to center) brought to 40 samples a
    /// symbol at unit gain, from a slice of the slot's spectrum.
    pub fn downsample(&mut self, f: f32) -> Vec<Complex32> {
        let baud = self.period.baud();
        let df = BASEBAND_HZ / self.l as f32;
        let sigbw = 4.0 * baud;
        let i0 = (f / df).round() as i64;
        let ih = ((f + 1.3 * sigbw / 2.0) / df).round() as i64;
        let nbw = (ih - i0 + 1) as usize;
        let nfft2 = self.m * NSS;
        let mut c = vec![Complex32::new(0.0, 0.0); nfft2];
        c[0] = self.bin(i0);
        for i in 1..=nbw.min(nfft2 / 2 - 1) {
            c[i] = self.bin(i0 + i as i64);
            c[nfft2 - i] = self.bin(i0 - i as i64);
        }
        let scale = 1.0 / self.l as f32;
        c.iter_mut().for_each(|v| *v *= scale);
        self.ifft.process_with_scratch(&mut c, &mut self.scratch);
        c
    }

    /// Noise density per Hz implied by a baseline value.
    pub fn n0(&self, base: f32) -> f32 {
        let nd = self.m / 2;
        let bins = (nd / 2) * 2 + 1;
        base / (4 * bins * self.l) as f32 / BASEBAND_HZ
    }
}

/// Fit a smooth noise floor (second-order in dB) through the lower envelope
/// of `s` over `ia..=ib`, as `fst4_baseline.f90` does: the lowest 30% of each
/// of eight segments. `s` is linear power; the result is too.
fn baseline(s: &[f32], ia: usize, ib: usize) -> Vec<f32> {
    let n = ib - ia + 1;
    let nlen = n / 8;
    let db: Vec<f32> = s[ia..=ib].iter().map(|&v| 10.0 * v.max(1e-20).log10()).collect();
    let mid = (ia + ib) as f64 / 2.0;
    let half = (ib - ia) as f64 / 2.0;
    let mut sums = [[0f64; 3]; 3];
    let mut y = [0f64; 3];
    for seg in 0..8 {
        let a = seg * nlen;
        let mut sorted = db[a..a + nlen].to_vec();
        sorted.sort_by(|x, z| x.total_cmp(z));
        let cut = sorted[(nlen * 3 / 10).min(nlen - 1)];
        for i in a..a + nlen {
            if db[i] <= cut {
                let x = (ia + i) as f64;
                let t = (x - mid) / half;
                let p = [1.0, t, t * t];
                for r in 0..3 {
                    y[r] += p[r] * db[i] as f64;
                    for c in 0..3 {
                        sums[r][c] += p[r] * p[c];
                    }
                }
            }
        }
    }
    let a = solve3(sums, y);
    let mut out = vec![0f32; s.len()];
    for (i, v) in out.iter_mut().enumerate() {
        let t = (i as f64 - mid) / half;
        *v = 10f32.powf((a[0] + t * (a[1] + t * a[2])) as f32 / 10.0 + 0.02);
    }
    out
}

fn solve3(mut a: [[f64; 3]; 3], mut b: [f64; 3]) -> [f64; 3] {
    for i in 0..3 {
        let p = (i..3).max_by(|&x, &y| a[x][i].abs().total_cmp(&a[y][i].abs())).expect("nonempty");
        a.swap(i, p);
        b.swap(i, p);
        if a[i][i].abs() < 1e-12 {
            continue;
        }
        for r in 0..3 {
            if r != i {
                let f = a[r][i] / a[i][i];
                for c in 0..3 {
                    a[r][c] -= f * a[i][c];
                }
                b[r] -= f * b[i];
            }
        }
    }
    let mut x = [0f64; 3];
    for i in 0..3 {
        x[i] = if a[i][i].abs() < 1e-12 { 0.0 } else { b[i] / a[i][i] };
    }
    x
}

/// The sync pattern shifted by `f` cycles per sample.
fn twist(sync: &[Complex32; SYNC_LEN], f: f32) -> Vec<Complex32> {
    let step = Complex32::from_polar(1.0, TAU * f);
    let mut ph = Complex32::new(1.0, 0.0);
    sync.iter()
        .map(|s| {
            let v = ph * s;
            ph *= step;
            v
        })
        .collect()
}

fn word_power(c: &[Complex32], r: &[Complex32]) -> f32 {
    let mut acc = Complex32::new(0.0, 0.0);
    for (x, y) in c.iter().zip(r) {
        acc += x * y.conj();
    }
    acc.norm() / SYNC_LEN as f32
}

fn sync_at(c: &[Complex32], start: usize, t1: &[Complex32], t2: &[Complex32]) -> f32 {
    let mut s = 0.0;
    for &(at, second) in &SYNC_AT {
        let from = start + at * NSS;
        let Some(seg) = c.get(from..from + SYNC_LEN) else { continue };
        s += word_power(seg, if second { t2 } else { t1 });
    }
    s
}

/// Best `(frequency offset in Hz, start sample at 40 per symbol)` around a
/// candidate: searched over +-1.2 tones by 0.1 and 0 to 3 s, then refined by
/// 0.02 tone and a sample, as `fst4_sync_search` does.
pub fn sync_search(c: &[Complex32], period: Period, refs: &Refs) -> (f32, usize) {
    let baud = period.baud();
    let fs2 = NSS as f32 * baud;
    let is0 = (1.5 * fs2).round() as i64;
    let ishw = is0;
    let last = (c.len() as i64 - (NN * NSS) as i64).max(1);
    let (mut sbest, mut fcbest, mut isbest) = (f32::NEG_INFINITY, 0f32, 1i64);
    for ifq in -12..=12 {
        let fc = 0.1 * baud * ifq as f32;
        let (t1, t2) = (twist(&refs.sync1, fc / fs2), twist(&refs.sync2, fc / fs2));
        let mut start = (is0 - ishw).max(1);
        while start <= (is0 + ishw).min(last) {
            let s = sync_at(c, start as usize, &t1, &t2);
            if s > sbest {
                (sbest, fcbest, isbest) = (s, fc, start);
            }
            start += 4;
        }
    }
    let (fc1, is1) = (fcbest, isbest);
    sbest = 0.0;
    for ifq in -7..=7 {
        let fc = fc1 + 0.02 * baud * ifq as f32;
        let (t1, t2) = (twist(&refs.sync1, fc / fs2), twist(&refs.sync2, fc / fs2));
        for start in (is1 - 4).max(1)..=(is1 + 4).min(last) {
            let s = sync_at(c, start as usize, &t1, &t2);
            if s > sbest {
                (sbest, fcbest, isbest) = (s, fc, start);
            }
        }
    }
    (fcbest, isbest as usize)
}

/// Each symbol correlated with each of the four tones.
pub type SymbolCorrs = Vec<[Complex32; 4]>;

pub fn tone_correlations(c: &[Complex32], start: usize, refs: &Refs) -> Option<SymbolCorrs> {
    let frame = c.get(start..start + NN * NSS)?;
    Some(
        frame
            .chunks_exact(NSS)
            .map(|sym| {
                let mut t = [Complex32::new(0.0, 0.0); 4];
                for (v, r) in t.iter_mut().zip(&refs.tones) {
                    for (x, y) in sym.iter().zip(r) {
                        *v += x * y.conj();
                    }
                }
                t
            })
            .collect(),
    )
}

/// For each of `nb` bits (first to last), the bigger amplitude among the
/// patterns with that bit set less the bigger among those with it clear.
/// `v` holds the squared amplitudes of all 2^nb patterns, highest-order bit
/// first in the pattern index.
fn bit_scores(v: &[f32], nb: usize, out: &mut [f32]) {
    let score = |one: f32, zero: f32| one.sqrt() - zero.sqrt();
    if nb <= 8 {
        for (ib, o) in out[..nb].iter_mut().enumerate() {
            let mask = 1usize << (nb - 1 - ib);
            let (mut one, mut zero) = (0f32, 0f32);
            for (i, &x) in v.iter().enumerate() {
                if i & mask != 0 {
                    one = one.max(x);
                } else {
                    zero = zero.max(x);
                }
            }
            *o = score(one, zero);
        }
        return;
    }
    // Sixteen bits: split in two halves, as each half's maxima are all that
    // a bit in the other half needs.
    let (mut hi, mut lo) = ([0f32; 256], [0f32; 256]);
    for (i, &x) in v.iter().enumerate() {
        hi[i >> 8] = hi[i >> 8].max(x);
        lo[i & 255] = lo[i & 255].max(x);
    }
    for (ib, o) in out[..16].iter_mut().enumerate() {
        let (table, bit) = if ib < 8 { (&hi, 7 - ib) } else { (&lo, 15 - ib) };
        let (mut one, mut zero) = (0f32, 0f32);
        for (i, &x) in table.iter().enumerate() {
            if i >> bit & 1 == 1 {
                one = one.max(x);
            } else {
                zero = zero.max(x);
            }
        }
        *o = score(one, zero);
    }
}

/// Normalized as `normalizebmet`: unit variance, the mean left in.
fn normalize(b: &mut [f32]) {
    let n = b.len() as f32;
    let av = b.iter().sum::<f32>() / n;
    let av2 = b.iter().map(|x| x * x).sum::<f32>() / n;
    let var = av2 - av * av;
    let sig = if var > 0.0 { var.sqrt() } else { av2.sqrt() };
    if sig > 0.0 {
        b.iter_mut().for_each(|x| *x /= sig);
    }
}

/// Soft bits of the 320 channel bits for blocks of 1, 2, 4 and 8 symbols
/// added coherently (the signal is phase-continuous, and every tone advances
/// half a turn per symbol, which the signs below undo), or `None` when the
/// sync is too poor to be worth a decode. Follows `get_fst4_bitmetrics.f90`.
pub fn bit_metrics(cs: &[[Complex32; 4]]) -> Option<[[f32; 320]; 4]> {
    let mut nsync = 0;
    for &(at, second) in &SYNC_AT {
        let w = if second { &SYNC2 } else { &SYNC1 };
        for k in 0..8 {
            let t = &cs[at + k];
            let top = (0..4).max_by(|&a, &b| t[a].norm_sqr().total_cmp(&t[b].norm_sqr())).expect("four");
            nsync += (top == w[k] as usize) as usize;
        }
    }
    if nsync < MIN_HARD_SYNC {
        return None;
    }
    let zero = Complex32::new(0.0, 0.0);
    let mut c1 = [[zero; 8]; 4];
    let mut c2 = [[zero; 4]; 16];
    let mut c4 = vec![[zero; 2]; 256];
    let mut s2 = vec![0f32; 65536];
    let mut out = [[0f32; 320]; 4];
    for k in (0..NN).step_by(8) {
        for m in 0..8 {
            for n in 0..4 {
                c1[n][m] = cs[k + m][GRAY[n]];
                s2[n] = c1[n][m].norm_sqr();
            }
            let ipt = k * 2 + 2 * m;
            bit_scores(&s2[..4], 2, &mut out[0][ipt..ipt + 2]);
        }
        for m in 0..4 {
            for i in 0..4 {
                for j in 0..4 {
                    let is = i * 4 + j;
                    c2[is][m] = c1[i][2 * m] - c1[j][2 * m + 1];
                    s2[is] = c2[is][m].norm_sqr();
                }
            }
            let ipt = k * 2 + 4 * m;
            bit_scores(&s2[..16], 4, &mut out[1][ipt..ipt + 4]);
        }
        for m in 0..2 {
            for i in 0..16 {
                for j in 0..16 {
                    let is = i * 16 + j;
                    c4[is][m] = c2[i][2 * m] + c2[j][2 * m + 1];
                    s2[is] = c4[is][m].norm_sqr();
                }
            }
            let ipt = k * 2 + 8 * m;
            bit_scores(&s2[..256], 8, &mut out[2][ipt..ipt + 8]);
        }
        for i in 0..256 {
            for j in 0..256 {
                s2[i * 256 + j] = (c4[i][0] + c4[j][1]).norm_sqr();
            }
        }
        let ipt = k * 2;
        bit_scores(&s2[..65536], 16, &mut out[3][ipt..ipt + 16]);
    }
    let qual = |seg: std::ops::Range<usize>, pat: [u8; 16]| {
        let seg_start = seg.start;
        seg.filter(|&i| (out[0][i] >= 0.0) as u8 == pat[i - seg_start]).count()
    };
    const P1: [u8; 16] = [0, 0, 0, 1, 1, 0, 1, 1, 0, 1, 0, 0, 1, 1, 1, 0];
    const P2: [u8; 16] = [1, 1, 1, 0, 0, 1, 0, 0, 1, 0, 1, 1, 0, 0, 0, 1];
    let q = qual(0..16, P1) + qual(76..92, P2) + qual(152..168, P1) + qual(228..244, P2) + qual(304..320, P1);
    if q < MIN_SYNC_QUAL {
        return None;
    }
    for b in &mut out {
        normalize(b);
        b.iter_mut().for_each(|x| *x *= 2.83);
    }
    Some(out)
}

/// The data bits' soft values from a block of metrics.
pub fn llrs(m: &[f32; 320]) -> [f32; ldpc::N] {
    let mut l = [0f32; ldpc::N];
    l[0..60].copy_from_slice(&m[16..76]);
    l[60..120].copy_from_slice(&m[92..152]);
    l[120..180].copy_from_slice(&m[168..228]);
    l[180..240].copy_from_slice(&m[244..304]);
    l
}

/// The frame's tones from a codeword.
pub fn tones_of(cw: &[u8; ldpc::N]) -> [u8; NN] {
    let mut data = [0u8; 120];
    for (i, d) in data.iter_mut().enumerate() {
        *d = match (cw[2 * i], cw[2 * i + 1]) {
            (0, 0) => 0,
            (0, 1) => 1,
            (1, 1) => 2,
            _ => 3,
        };
    }
    let mut t = [0u8; NN];
    t[..8].copy_from_slice(&SYNC1);
    t[8..38].copy_from_slice(&data[..30]);
    t[38..46].copy_from_slice(&SYNC2);
    t[46..76].copy_from_slice(&data[30..60]);
    t[76..84].copy_from_slice(&SYNC1);
    t[84..114].copy_from_slice(&data[60..90]);
    t[114..122].copy_from_slice(&SYNC2);
    t[122..152].copy_from_slice(&data[90..120]);
    t[152..160].copy_from_slice(&SYNC1);
    t
}

/// Signal to noise in 2500 Hz from the tones known sent.
pub fn snr_db(cs: &[[Complex32; 4]], tones: &[u8; NN], n0: f32, baud: f32) -> f32 {
    let avg = cs.iter().zip(tones).map(|(c, &t)| c[t as usize].norm_sqr()).sum::<f32>() / NN as f32;
    let a2 = avg / (NSS * NSS) as f32 - n0 * baud;
    if a2 <= 0.0 || n0 <= 0.0 {
        return -40.0;
    }
    10.0 * (a2 / (n0 * 2500.0)).log10()
}

/// Residual frequency (tones) and total frequency change across the frame
/// (tones), fit to the known tones. Good to about +-0.8 tone: past that the
/// symbol-to-symbol phase step at the frame's ends reaches half a turn and
/// the fit aliases, so the search stops there.
pub fn fit_residual(cs: &[[Complex32; 4]], tones: &[u8; NN]) -> (f32, f32) {
    let r: Vec<Complex32> =
        cs.iter().zip(tones).enumerate().map(|(k, (c, &t))| c[t as usize] * if k % 2 == 0 { 1.0 } else { -1.0 }).collect();
    let (mut bdf, mut bdrift, mut best) = (0f32, 0f32, -1f32);
    for idf in -15..=15 {
        for idr in -16..=16 {
            let df = idf as f32 * 0.02;
            let dr = idr as f32 * 0.05;
            let mut acc = Complex32::new(0.0, 0.0);
            for (k, v) in r.iter().enumerate() {
                let t = k as f32 - NN as f32 / 2.0;
                acc += v * Complex32::from_polar(1.0, -TAU * (df * t + 0.5 * dr * t * t / NN as f32));
            }
            let p = acc.norm_sqr();
            if p > best {
                (bdf, bdrift, best) = (df, dr, p);
            }
        }
    }
    (bdf, bdrift)
}
