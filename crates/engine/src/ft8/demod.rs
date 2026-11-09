//! From audio to codewords: a waterfall of the slot, a search for the Costas
//! sync patterns in it, and soft bits from the tones between them.
//!
//! The waterfall is oversampled twice in time and twice in frequency, so a
//! signal starting between symbol boundaries or sitting between tone bins
//! still finds a view of itself that lines up within a quarter of either.

use super::Protocol;
use super::ldpc::{self, N};
use rustfft::{FftPlanner, num_complex::Complex32};

pub const TIME_OSR: usize = 2;
pub const FREQ_OSR: usize = 2;

pub(super) const FT8_COSTAS: [u8; 7] = [3, 1, 4, 0, 6, 5, 2];
pub(super) const FT4_COSTAS: [[u8; 4]; 4] = [[0, 1, 3, 2], [1, 0, 2, 3], [2, 3, 1, 0], [3, 2, 0, 1]];
pub(super) const FT8_GRAY: [u8; 8] = [0, 1, 3, 2, 5, 6, 4, 7];
pub(super) const FT4_GRAY: [u8; 4] = [0, 1, 3, 2];
/// FT4 scrambles the payload so a CQ is not a long run of zeros.
pub const FT4_XOR: [u8; 10] = [0x4a, 0x5e, 0x89, 0xb4, 0xb0, 0x8a, 0x79, 0x55, 0xbe, 0x28];

impl Protocol {
    pub(super) fn symbol_period(self) -> f32 {
        match self {
            Protocol::Ft8 => 0.160,
            Protocol::Ft4 => 0.048,
        }
    }
    pub(super) fn tones(self) -> usize {
        match self {
            Protocol::Ft8 => 8,
            Protocol::Ft4 => 4,
        }
    }
    /// Channel symbols in a transmission.
    pub(super) fn symbols(self) -> usize {
        match self {
            Protocol::Ft8 => 79,
            Protocol::Ft4 => 105,
        }
    }
    /// Where in the transmission data symbol `k` sits, past the syncs.
    fn data_symbol(self, k: usize) -> usize {
        match self {
            Protocol::Ft8 => k + if k < 29 { 7 } else { 14 },
            Protocol::Ft4 => k + if k < 29 { 5 } else if k < 58 { 9 } else { 13 },
        }
    }
    /// Range of whole-symbol start offsets searched, relative to the slot.
    /// FT8's symbols are long enough that ±2 s is a few dozen; FT4 needs
    /// more of its short ones for a similar spread.
    fn time_search(self) -> std::ops::Range<i32> {
        match self {
            Protocol::Ft8 => -10..20,
            Protocol::Ft4 => -12..48,
        }
    }
    fn gfsk_bt(self) -> f32 {
        match self {
            Protocol::Ft8 => 2.0,
            Protocol::Ft4 => 1.0,
        }
    }
}

/// Channel tones for a payload: sync patterns and Gray-coded data.
pub fn tones(protocol: Protocol, payload: &[u8; 10]) -> Vec<u8> {
    let mut p = *payload;
    if protocol == Protocol::Ft4 {
        for (b, x) in p.iter_mut().zip(FT4_XOR) {
            *b ^= x;
        }
    }
    let cw = ldpc::encode(&ldpc::add_crc(&p));
    let mut out = vec![0u8; protocol.symbols()];
    match protocol {
        Protocol::Ft8 => {
            for s in [0, 36, 72] {
                out[s..s + 7].copy_from_slice(&FT8_COSTAS);
            }
            for k in 0..58 {
                let bits = (cw[3 * k] << 2) | (cw[3 * k + 1] << 1) | cw[3 * k + 2];
                out[protocol.data_symbol(k)] = FT8_GRAY[bits as usize];
            }
        }
        Protocol::Ft4 => {
            for (m, s) in [1, 34, 67, 100].into_iter().enumerate() {
                out[s..s + 4].copy_from_slice(&FT4_COSTAS[m]);
            }
            for k in 0..87 {
                let bits = (cw[2 * k] << 1) | cw[2 * k + 1];
                out[protocol.data_symbol(k)] = FT4_GRAY[bits as usize];
            }
        }
    }
    out
}

/// Spectral power over the slot, in dB, for every symbol-long step and its
/// half-step, and every tone bin and its half-bin.
pub struct Waterfall {
    pub protocol: Protocol,
    pub num_blocks: usize,
    pub num_bins: usize,
    /// The first tone bin kept, in tone spacings from 0 Hz.
    pub min_bin: usize,
    /// Samples per symbol and per FFT.
    pub block: usize,
    pub nfft: usize,
    mag: Vec<f32>,
}

impl Waterfall {
    pub fn new(protocol: Protocol, audio: &[f32], rate: f32, f_min: f32, f_max: f32) -> Waterfall {
        let period = protocol.symbol_period();
        let block = (rate * period).round() as usize;
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
                // The window ends where this step's new samples end, as a
                // streaming analysis would see it.
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
                        let c = buf[bin * FREQ_OSR + fs];
                        mag[at] = 10.0 * (1e-12 + c.norm_sqr()).log10();
                        at += 1;
                    }
                }
            }
        }
        Waterfall { protocol, num_blocks, num_bins, min_bin, block, nfft, mag }
    }

    #[inline]
    fn at(&self, block: usize, c: &Candidate, tone: usize) -> f32 {
        self.mag[((block * TIME_OSR + c.time_sub) * FREQ_OSR + c.freq_sub) * self.num_bins + c.freq_offset + tone]
    }

    /// Audio frequency of a candidate's lowest tone.
    pub fn freq_hz(&self, c: &Candidate) -> f32 {
        (self.min_bin as f32 + c.freq_offset as f32 + c.freq_sub as f32 / FREQ_OSR as f32) / self.protocol.symbol_period()
    }

    /// Where a candidate's first symbol starts, in seconds from the slot's
    /// start. Each FFT is centered half its length before the end of the
    /// step it belongs to; a symbol lines up when its center does.
    pub fn start_s(&self, c: &Candidate, rate: f32) -> f32 {
        let sub = (self.block / TIME_OSR) as f32;
        let center = c.time_offset as f32 * self.block as f32 + (c.time_sub as f32 + 1.0) * sub - self.nfft as f32 / 2.0;
        (center - self.block as f32 / 2.0) / rate
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Candidate {
    pub score: f32,
    pub time_offset: i32,
    pub freq_offset: usize,
    pub time_sub: usize,
    pub freq_sub: usize,
}

impl Waterfall {
    /// How strongly the sync tones stand out where a candidate expects
    /// them: each against its neighbors one tone either side and one
    /// symbol either side, averaged, in half-dB. Comparing against
    /// neighbors rather than the whole band keeps it level-independent.
    fn sync_score(&self, c: &Candidate) -> f32 {
        let (groups, len): (&[usize], usize) = match self.protocol {
            Protocol::Ft8 => (&[0, 36, 72], 7),
            Protocol::Ft4 => (&[1, 34, 67, 100], 4),
        };
        let tones = self.protocol.tones();
        let mut score = 0.0;
        let mut n = 0;
        for (m, &g) in groups.iter().enumerate() {
            for k in 0..len {
                let b = c.time_offset + (g + k) as i32;
                if b < 0 {
                    continue;
                }
                if b as usize >= self.num_blocks {
                    break;
                }
                let b = b as usize;
                let sm = match self.protocol {
                    Protocol::Ft8 => FT8_COSTAS[k] as usize,
                    Protocol::Ft4 => FT4_COSTAS[m][k] as usize,
                };
                let p = self.at(b, c, sm);
                if sm > 0 {
                    score += p - self.at(b, c, sm - 1);
                    n += 1;
                }
                if sm + 1 < tones {
                    score += p - self.at(b, c, sm + 1);
                    n += 1;
                }
                if k > 0 && b > 0 {
                    score += p - self.at(b - 1, c, sm);
                    n += 1;
                }
                if k + 1 < len && b + 1 < self.num_blocks {
                    score += p - self.at(b + 1, c, sm);
                    n += 1;
                }
            }
        }
        if n == 0 { 0.0 } else { 2.0 * score / n as f32 }
    }

    /// The strongest `max` sync candidates scoring at least `min_score`,
    /// strongest first.
    ///
    /// With `distinct`, a candidate within half a tone and half a symbol of
    /// a stronger one is left out. A signal scores at several neighboring
    /// offsets of the oversampled grid, and on a busy band the strong
    /// signals' repeats filled the list before the weak ones got a place.
    pub fn candidates(&self, max: usize, min_score: f32, distinct: bool) -> Vec<Candidate> {
        let tones = self.protocol.tones();
        let mut out = Vec::new();
        for time_sub in 0..TIME_OSR {
            for freq_sub in 0..FREQ_OSR {
                for time_offset in self.protocol.time_search() {
                    for freq_offset in 0..self.num_bins.saturating_sub(tones - 1) {
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
        if !distinct {
            out.truncate(max);
            return out;
        }
        // Positions in steps of the oversampled grid.
        let at = |c: &Candidate| (c.time_offset * TIME_OSR as i32 + c.time_sub as i32, (c.freq_offset * FREQ_OSR + c.freq_sub) as i32);
        let mut kept: Vec<Candidate> = Vec::with_capacity(max);
        for c in out {
            let (t, f) = at(&c);
            let shadowed = kept.iter().any(|k| {
                let (kt, kf) = at(k);
                (kt - t).abs() <= TIME_OSR as i32 / 2 && (kf - f).abs() <= FREQ_OSR as i32 / 2
            });
            if !shadowed {
                kept.push(c);
                if kept.len() == max {
                    break;
                }
            }
        }
        kept
    }

    /// Soft bits, log(P(1)/P(0)), from the tones' powers: for each bit the
    /// strongest tone that would mean a one against the strongest that
    /// would mean a zero. Scaled to the spread the decoder was tuned for.
    pub fn llrs(&self, c: &Candidate, spread: f32) -> [f32; N] {
        let mut llr = [0f32; N];
        let p = self.protocol;
        let (nd, bps) = match p {
            Protocol::Ft8 => (58, 3),
            Protocol::Ft4 => (87, 2),
        };
        let gray: &[u8] = match p {
            Protocol::Ft8 => &FT8_GRAY,
            Protocol::Ft4 => &FT4_GRAY,
        };
        // Amplitude at the tone each bit pattern is sent as, symbol by
        // symbol; none where the transmission runs off the slot.
        let mut ms = [[0f32; 8]; 87];
        for (k, m) in ms.iter_mut().enumerate().take(nd) {
            let b = c.time_offset + p.data_symbol(k) as i32;
            if b < 0 || b as usize >= self.num_blocks {
                continue;
            }
            for (j, v) in m.iter_mut().enumerate().take(p.tones()) {
                *v = 10f32.powf(self.at(b as usize, c, gray[j] as usize) / 20.0);
            }
        }
        let rms: Vec<f32> = ms[..nd].iter().map(|m| (m.iter().map(|v| v * v).sum::<f32>() / p.tones() as f32).sqrt()).collect();
        for (m, g) in ms.iter_mut().zip(loud_symbol_gains(&rms)) {
            m.iter_mut().for_each(|v| *v *= g);
        }
        for (k, m) in ms.iter().enumerate().take(nd) {
            for i in 0..bps {
                let mask = 1 << (bps - 1 - i);
                let (mut one, mut zero) = (0f32, 0f32);
                for (j, &v) in m.iter().enumerate().take(p.tones()) {
                    if j & mask != 0 { one = one.max(v) } else { zero = zero.max(v) }
                }
                llr[bps * k + i] = one - zero;
            }
        }
        let (sum, sum2) = llr.iter().fold((0.0f32, 0.0f32), |(a, b), v| (a + v, b + v * v));
        let var = (sum2 - sum * sum / N as f32) / N as f32;
        if var > 0.0 {
            let k = (spread / var).sqrt();
            llr.iter_mut().for_each(|v| *v *= k);
        }
        llr
    }
}

/// Brings each symbol much louder than the transmission's median one down
/// to a little above the median's level. A static crash or a strong
/// neighbor's keying lights every tone of the few symbols it lands on; in
/// amplitude those symbols' soft bits come out many times the size of the
/// rest, and the scaling to a fixed spread then shrinks every clean bit
/// toward nothing, so a few crashes sank codewords that were otherwise
/// sound. Scaled down, the struck symbols say little either way, which is
/// what they know.
///
/// The ceiling, 1.2 times the median, is from the fixed-seed sweeps in the
/// tests (12 slots of 5 signals at each of 7 SNRs around the threshold):
/// in steady noise FT8 decodes 174 of 420 with or without it, FT4 139, and
/// JS8 117 against 115; under four 15 ms crashes FT8 126 instead of 85,
/// FT4 96 instead of 31, JS8 65 instead of 40. Capping at the median
/// itself gained a little more under crashes and lost a JS8 decode in
/// steady noise; 1.5 times gave back some of the crash gain.
///
/// Takes each symbol's RMS tone amplitude; returns the gain for each. The
/// deep pass and JS8 use it too.
pub(crate) fn loud_symbol_gains(rms: &[f32]) -> Vec<f32> {
    const CEILING: f32 = 1.2;
    let mut sorted: Vec<f32> = rms.iter().copied().filter(|&r| r > 0.0).collect();
    if sorted.is_empty() {
        return vec![1.0; rms.len()];
    }
    let mid = sorted.len() / 2;
    let ceiling = CEILING * *sorted.select_nth_unstable_by(mid, f32::total_cmp).1;
    rms.iter().map(|&r| if r > ceiling { ceiling / r } else { 1.0 }).collect()
}

/// Decode one candidate to its 77-bit payload, or `None` if the code or
/// the CRC rejects it.
pub fn decode_candidate(wf: &Waterfall, c: &Candidate, iters: usize, spread: f32) -> Option<([u8; 10], usize)> {
    let llr = wf.llrs(c, spread);
    let (cw, errors) = ldpc::bp_decode(&llr, iters);
    if errors > 0 {
        return None;
    }
    let a91 = ldpc::pack91(&cw);
    if !ldpc::crc_ok(&a91) {
        return None;
    }
    let mut payload = [0u8; 10];
    payload.copy_from_slice(&a91[..10]);
    payload[9] &= 0xf8;
    if wf.protocol == Protocol::Ft4 {
        for (b, x) in payload.iter_mut().zip(FT4_XOR) {
            *b ^= x;
        }
    }
    Some((payload, errors))
}

/// erf, to 1.5e-7 (Abramowitz and Stegun 7.1.26); the pulse shape needs
/// no more and std has none.
fn erf(x: f32) -> f32 {
    let t = 1.0 / (1.0 + 0.327_591_1 * x.abs());
    let y = 1.0 - (((((1.061_405_4 * t - 1.453_152_1) * t) + 1.421_413_7) * t - 0.284_496_74) * t + 0.254_829_6) * t * (-x * x).exp();
    if x >= 0.0 { y } else { -y }
}

/// Continuous-phase Gaussian-filtered FSK for a tone sequence, unit
/// amplitude, at `rate`, the first and last symbols ramped.
pub fn gfsk(protocol: Protocol, tones: &[u8], f0: f32, rate: f32) -> Vec<f32> {
    gfsk_complex(protocol, tones, f0, rate).into_iter().map(|c| c.im).collect()
}

/// [`gfsk`] as a complex exponential, whose imaginary part is the
/// transmission: the reference a decoded signal is subtracted with.
pub fn gfsk_complex(protocol: Protocol, tones: &[u8], f0: f32, rate: f32) -> Vec<Complex32> {
    let sps = (rate * protocol.symbol_period()).round() as usize;
    let n = tones.len() * sps;
    let bt = protocol.gfsk_bt();
    const K: f32 = 5.336_446; // π √(2 / ln 2)
    let pulse: Vec<f32> = (0..3 * sps)
        .map(|i| {
            let t = i as f32 / sps as f32 - 1.5;
            (erf(K * bt * (t + 0.5)) - erf(K * bt * (t - 0.5))) / 2.0
        })
        .collect();
    let peak = std::f32::consts::TAU / sps as f32;
    let mut dphi = vec![std::f32::consts::TAU * f0 / rate; n + 2 * sps];
    for (i, &t) in tones.iter().enumerate() {
        for (j, p) in pulse.iter().enumerate() {
            dphi[i * sps + j] += peak * t as f32 * p;
        }
    }
    // Dummy symbols either end, repeating the first and last tones.
    let (first, last) = (tones[0] as f32, tones[tones.len() - 1] as f32);
    for j in 0..2 * sps {
        dphi[j] += peak * pulse[j + sps] * first;
        dphi[j + n] += peak * pulse[j] * last;
    }
    let mut phi = 0.0f64;
    let mut out = Vec::with_capacity(n);
    for k in 0..n {
        out.push(Complex32::new(phi.cos() as f32, phi.sin() as f32));
        phi = (phi + dphi[k + sps] as f64) % std::f64::consts::TAU;
    }
    let ramp = sps / 8;
    for i in 0..ramp {
        let env = (1.0 - (std::f32::consts::TAU * i as f32 / (2 * ramp) as f32).cos()) / 2.0;
        out[i] *= env;
        out[n - 1 - i] *= env;
    }
    out
}

