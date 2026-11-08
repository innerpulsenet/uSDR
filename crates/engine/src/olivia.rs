//! Olivia MFSK (Pawel Jalocha, SP9VRC, 2005), decoded the way its author's
//! receiver does it, from the published description and fldigi's copy of
//! his code.
//!
//! An Olivia signal is T tones (4 to 64) filling a bandwidth B (125 to
//! 2000 Hz), each tone B/T apart, one tone at a time at B/T baud. The audio
//! is taken to 8 kHz, where a symbol's shaping window is N samples long,
//! 2 to the power log2 T + 7 - log2(B/125) (a raised cosine over two
//! symbol periods, so neighbors overlap by half), and the tones sit two
//! bins of an N-point FFT apart, centered in the band: tone k at the
//! band's lower edge plus (k + 1/2) B/T.
//!
//! Characters travel in blocks of 64 symbols. A block of log2 T characters
//! (seven bits each) is spread so that every character touches every
//! symbol: each character becomes one row of a 64 by 64 Walsh-Hadamard
//! matrix (its low six bits pick the row, the seventh its sign), the row is
//! scrambled by a fixed 64-bit sequence started 13 places further on for
//! each character, and bit t of character c's row becomes bit (c + t) mod
//! log2 T of symbol t. The symbol is Gray coded onto a tone. Nothing marks
//! where a block starts.
//!
//! The receiver works on a grid: an FFT every quarter of a symbol, zero
//! padded to give bins a quarter of the tone spacing apart, so a signal
//! off its nominal frequency still has a bin within an eighth of a tone
//! spacing. For every quarter-symbol instant and every frequency offset in
//! the search range, each symbol's tone powers become a log-likelihood for
//! each of its bits (against a noise floor measured between the tones),
//! and the 64 symbols before that instant are read as a block: descrambled
//! and passed through a fast Hadamard transform, which correlates them with
//! all 128 possible characters at once. The best correlation gives the
//! character and, against the others, how sure it is. The block reading
//! that is surest, smoothed over the last few blocks at the same alignment,
//! is where the blocks start and what frequency the signal is on; noise has
//! no alignment that stays sure, and prints nothing.
//!
//! With several settings side by side ([`OliviaDecoder::new_auto`]) each
//! has its own receiver; a strong signal of one can look like text to
//! another, so a block waits a few seconds and is dropped if another
//! setting fit the signal clearly better meanwhile.
//!
//! Measured on the test channel (a second of clock error, 30 Hz off), the
//! SNR in 2500 Hz where fewer than 2% of characters are wrong: 8/500
//! -12 dB, 16/500 and 32/1000 -13 dB, 8/250 -15 dB; 8/500 in flat Rayleigh
//! fading (1 Hz) -4 dB. Half an hour of noise prints nothing.
//!
//! Contestia is the same waveform and the same receiver with a shorter
//! code: blocks of 32 symbols, six-bit characters (upper case, figures and
//! a little punctuation, from a table of its own), and its own scrambling
//! sequence started 5 places further on for each character.
//!
//! Not decoded: signals sent with the tones reversed (lower sideband).

use crate::mfsk::resample::Resampler;
use crate::rtty::Lines;
use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

/// The rate Olivia is defined at.
const RATE: f32 = 8000.0;

/// Which of the two codes on the Olivia waveform.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Family {
    #[default]
    Olivia,
    Contestia,
}

impl Family {
    /// Symbols in a block, and the length of the Walsh-Hadamard code.
    fn block(self) -> usize {
        match self {
            Family::Olivia => 64,
            Family::Contestia => 32,
        }
    }

    /// The scrambling sequence, from Jalocha's encoder.
    fn scramble(self) -> u64 {
        match self {
            Family::Olivia => 0xE257_E6D0_2915_74EC,
            Family::Contestia => 0xEDB8_8320,
        }
    }

    /// Each character's scrambling starts this many places after the last's.
    fn scramble_shift(self) -> usize {
        match self {
            Family::Olivia => 13,
            Family::Contestia => 5,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Family::Olivia => "Olivia",
            Family::Contestia => "Contestia",
        }
    }

    /// A decoded code word as text; `None` for what prints nothing.
    fn to_char(self, v: u8) -> Option<char> {
        match self {
            Family::Olivia => match v {
                b'\r' | b'\n' => Some('\n'),
                0x20..=0x7e => Some(v as char),
                _ => None,
            },
            // The transmitter's table read backward. 0 is both line feed and
            // the idle filler, and carriage return already ends the line; 61
            // is backspace, which a log has no use for; 62 and 63 are never
            // sent.
            Family::Contestia => match v {
                1..=58 => Some((v + 32) as char),
                59 => Some(' '),
                60 => Some('\n'),
                _ => None,
            },
        }
    }

    /// How sure a block must be, against [`SCORE_MIN`]'s scale. Contestia's
    /// blocks have half the symbols, so they gather half the evidence.
    fn score_scale(self) -> f32 {
        match self {
            Family::Olivia => 1.0,
            Family::Contestia => CONTESTIA_SCORE_SCALE,
        }
    }

    /// The least share of the evidence a reading must fit. Half the symbols
    /// spread the best of the 64 correlations further in noise.
    fn agreement_min(self) -> f32 {
        match self {
            Family::Olivia => AGREEMENT_MIN,
            Family::Contestia => CONTESTIA_AGREEMENT_MIN,
        }
    }
}
/// The search reaches this far either side of the configured center. A
/// station further off than this is off by more than operators leave one;
/// the cost of the search grows with it.
const SEARCH_HZ: f32 = 100.0;
/// Spectra per symbol: timing is tried at a quarter symbol.
const PHASES: usize = 4;
/// Zero padding of the FFT: bins a quarter of the tone spacing apart.
const PAD: usize = 2;

/// A block's score is its own plus this share of the last at the same
/// alignment, so a block in a fade is helped by the ones before it to
/// keep the alignment, and noise, which never repeats, gains nothing.
const SCORE_MEMORY: f32 = 0.5;
/// A block is printed when its smoothed score, the mean over its
/// characters of the log odds that each is right (against the runner-up),
/// reaches this for three characters a block, and this times the square
/// root of 3 over the characters a block otherwise: the mean of fewer
/// characters spreads more. In 30 minutes of noise the best of every
/// alignment and offset reached 12.9 for 4/250, 11.9 for 8/250, 10.9 for
/// 8/500, 10.0 for 16/500, 9.5 for 16/1000 and 8.3 for 32/1000, against
/// 18.4, 15, 15, 13, 13 and 11.6 here.
const SCORE_MIN: f32 = 15.0;
/// Contestia's score threshold against Olivia's. In ten minutes of noise
/// the best of every alignment and offset reached 9.6 for 4/250, 8.1 for
/// 8/250 and 6.8 for 32/1000, against 18.4, 15 and 11.6 with this. Noise
/// alone would allow less; on the air, 14.071 MHz with FT8 in the passband
/// printed fragments until it was this high and the agreement 0.75.
const CONTESTIA_SCORE_SCALE: f32 = 1.0;
/// ...and its agreement threshold, set on the air likewise.
const CONTESTIA_AGREEMENT_MIN: f32 = 0.75;
/// A second good block must fall this close to a block's frequency.
const CORROBORATE_HZ: f32 = 6.0;
/// ...and the reading's characters agree with this share of the evidence
/// at least: the best correlation against the most any character could
/// have, every bit agreeing. A signal of another setting (an 8/500 read
/// as 16/500, say) can make a structured, confident looking reading, but
/// one that fits its symbols only about 40% of the way; a true signal
/// fits more than 95% of the way at -8 dB, about 60% at -14 dB, where
/// noise alone also reaches 60% but never the score.
const AGREEMENT_MIN: f32 = 0.55;
/// A character inside a printed block is printed if the odds it is right
/// reach this (log odds), whatever its neighbors did.
const CHAR_LOG_ODDS_MIN: f32 = 2.0;
/// Log-likelihood ratios are limited to this, so one symbol in a static
/// crash cannot outvote the rest of a character's 64.
const LLR_MAX: f32 = 12.0;
/// The noise floor follows over about this many spectra.
const NOISE_TAU_HOPS: f32 = 32.0;
/// Locked while a block was printed this recently (in blocks).
const LOCK_HOLD_BLOCKS: u64 = 3;
/// Pending text is let go after this long without a character.
const FLUSH_S: f32 = 4.0;
/// With several settings side by side, a block waits this long before it
/// is printed (longer than the slowest common setting takes to judge its
/// first block)...
const HOLD_S: f32 = 4.0;
/// ...and is dropped if another setting's receiver read a block meanwhile
/// that fit its symbols this much better.
const BETTER_FIT: f32 = 0.1;
/// What a block's sent tones show against a steady tone of the same mean
/// power (see `snr_from`).
const SNR_FACTOR: f32 = 7.2;

/// One Olivia setting.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OliviaConfig {
    /// 4, 8, 16, 32 or 64.
    pub tones: u32,
    /// 125, 250, 500, 1000 or 2000.
    pub bandwidth_hz: f32,
    /// The middle of the band in the audio.
    pub center_hz: f32,
    /// Olivia or Contestia.
    pub family: Family,
}

impl Default for OliviaConfig {
    fn default() -> Self {
        Self {
            tones: 8,
            bandwidth_hz: 500.0,
            center_hz: 1500.0,
            family: Family::Olivia,
        }
    }
}

impl OliviaConfig {
    pub fn new(tones: u32, bandwidth_hz: f32, center_hz: f32) -> Self {
        Self {
            tones,
            bandwidth_hz,
            center_hz,
            family: Family::Olivia,
        }
    }

    /// One Contestia setting.
    pub fn contestia(tones: u32, bandwidth_hz: f32, center_hz: f32) -> Self {
        Self {
            family: Family::Contestia,
            ..Self::new(tones, bandwidth_hz, center_hz)
        }
    }

    /// Contestia's settings as they are heard, all at one center.
    pub fn common_contestia(center_hz: f32) -> Vec<OliviaConfig> {
        [
            (4, 250.0),
            (8, 250.0),
            (4, 500.0),
            (8, 500.0),
            (16, 500.0),
            (8, 1000.0),
            (16, 1000.0),
            (32, 1000.0),
        ]
        .into_iter()
        .map(|(t, b)| Self::contestia(t, b, center_hz))
        .collect()
    }

    /// The settings heard most on the air, all at one center: what the
    /// automatic decoder tries side by side.
    pub fn common(center_hz: f32) -> Vec<OliviaConfig> {
        [
            (8, 250.0),
            (8, 500.0),
            (16, 500.0),
            (32, 1000.0),
            (16, 1000.0),
            (4, 250.0),
        ]
        .into_iter()
        .map(|(t, b)| Self::new(t, b, center_hz))
        .collect()
    }

    /// Tones clamped to 4..64 and the bandwidth to 125..2000 Hz, both
    /// rounded down to a power of two (times 125 Hz for the bandwidth), as
    /// Jalocha's code does.
    fn normalized(self) -> Self {
        let tones = 1u32 << (self.tones.clamp(4, 64) as f32).log2().floor() as u32;
        let b = (self.bandwidth_hz / 125.0).clamp(1.0, 16.0);
        let bandwidth_hz = 125.0 * (1u32 << b.log2().floor() as u32) as f32;
        Self {
            tones,
            bandwidth_hz,
            center_hz: self.center_hz,
            family: self.family,
        }
    }

    /// Characters per block.
    fn bits(&self) -> usize {
        self.tones.trailing_zeros() as usize
    }

    /// The symbol shaping length at 8 kHz.
    fn symbol_len(&self) -> usize {
        let b = (self.bandwidth_hz / 125.0).round() as u32;
        1usize << (self.bits() + 7 - b.trailing_zeros() as usize)
    }

    pub fn baud(&self) -> f32 {
        self.bandwidth_hz / self.tones as f32
    }

    pub fn label(&self) -> String {
        format!("{}/{}", self.tones, self.bandwidth_hz as u32)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum OliviaEvent {
    /// Decoded text, a line at a time (or what there is after a pause).
    Text {
        text: String,
        tones: u32,
        bandwidth_hz: u32,
        center_hz: f32,
        snr_db: f32,
    },
}

impl OliviaEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            OliviaEvent::Text { .. } => "text",
        }
    }

    pub fn summary(&self) -> String {
        match self {
            OliviaEvent::Text {
                text,
                tones,
                bandwidth_hz,
                ..
            } => format!("{tones}/{bandwidth_hz}: {text}"),
        }
    }

    pub fn fields(&self) -> Vec<(&'static str, String)> {
        match self {
            OliviaEvent::Text {
                text,
                tones,
                bandwidth_hz,
                center_hz,
                snr_db,
            } => vec![
                ("text", text.clone()),
                ("tones", tones.to_string()),
                ("bandwidthHz", bandwidth_hz.to_string()),
                ("centerHz", format!("{center_hz:.1}")),
                ("snrDb", format!("{snr_db:.1}")),
            ],
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct OliviaStatus {
    /// A block was printed within the last few block lengths.
    pub locked: bool,
    /// Where the signal's band center is, as last found (the configured
    /// center when nothing has been found).
    pub center_hz: f32,
    /// Signal to noise in 2500 Hz, measured on the last printed block's
    /// own tones; None until a block is printed.
    pub snr_db: Option<f32>,
    /// The setting the signal was found with (the first one tried when
    /// none has been).
    pub tones: u32,
    pub bandwidth_hz: u32,
    /// Characters printed since the decoder was made.
    pub chars: u64,
    /// Smoothed score of the last printed block (mean log odds a
    /// character is right).
    pub quality: f32,
}

// ---------------------------------------------------------------------------
// The code.

/// Jalocha's fast Hadamard transform, signs and all.
fn fht(d: &mut [f32]) {
    let len = d.len();
    let mut step = 1;
    while step < len {
        for ptr in (0..len).step_by(2 * step) {
            for p in ptr..ptr + step {
                let (a, b) = (d[p], d[p + step]);
                d[p] = b + a;
                d[p + step] = b - a;
            }
        }
        step *= 2;
    }
}

/// Its inverse (times the length), which the transmitter uses. Jalocha's
/// butterflies are not symmetric, so the forward transform of a unit
/// vector is a different row; only the inverse matches what is sent.
fn ifht(d: &mut [f32]) {
    let len = d.len();
    let mut step = len / 2;
    while step > 0 {
        for ptr in (0..len).step_by(2 * step) {
            for p in ptr..ptr + step {
                let (a, b) = (d[p], d[p + step]);
                d[p] = a - b;
                d[p + step] = a + b;
            }
        }
        step /= 2;
    }
}

/// Is bit t of character c's row inverted by the scrambler?
fn scrambled(f: Family, c: usize, t: usize) -> bool {
    (f.scramble() >> ((c * f.scramble_shift() + t) & (f.block() - 1))) & 1 == 1
}

fn gray(x: usize) -> usize {
    x ^ (x >> 1)
}

fn ungray(mut g: usize) -> usize {
    let mut x = 0;
    while g != 0 {
        x ^= g;
        g >>= 1;
    }
    x
}

/// A block of characters as the symbols (before Gray coding) that carry it.
fn encode_block(f: Family, chars: &[u8], bits: usize) -> Vec<usize> {
    let block = f.block();
    let mut out = vec![0usize; block];
    let mut row = vec![0f32; block];
    for (c, &ch) in chars.iter().enumerate().take(bits) {
        let ch = ch as usize & (2 * block - 1);
        // The row a character picks: the inverse transform of a unit vector
        // at its low six bits, negated by its seventh.
        row.fill(0.0);
        if ch < block {
            row[ch] = 1.0;
        } else {
            row[ch - block] = -1.0;
        }
        ifht(&mut row);
        for (t, v) in row.iter().enumerate() {
            let neg = (*v < 0.0) != scrambled(f, c, t);
            if neg {
                out[t] |= 1 << ((c + t) % bits);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// One setting's receiver.

/// What one block reading found.
#[derive(Clone, Copy, Default)]
struct Reading {
    chars: [u8; 6],
    log_odds: [f32; 6],
    score: f32,
    /// The best correlation against the most any character could have
    /// (every bit agreeing), averaged over the characters.
    agreement: f32,
}

struct Chain {
    cfg: OliviaConfig,
    family: Family,
    /// Symbols in a block, and the spectra a block spans.
    block: usize,
    block_hops: usize,
    /// Spectra kept: a block, plus the half block a reading waits before it
    /// is judged, with room to spare.
    ring_hops: usize,
    tones: usize,
    bits: usize,
    n: usize,
    hop: usize,
    fft: Arc<dyn Fft<f32>>,
    win: Vec<f32>,
    buf: Vec<Complex32>,
    scratch: Vec<Complex32>,
    /// The last `n` samples, oldest first after `pos`.
    ring: Vec<f32>,
    pos: usize,
    since_hop: usize,
    /// Bin of tone 0 at the lowest offset tried.
    lo_bin: usize,
    /// Offsets tried, one bin apart.
    offsets: usize,
    /// Bins kept per spectrum: from `lo_bin` to the top tone at the
    /// highest offset.
    width: usize,
    bin_hz: f32,
    /// Tone powers, `width` per spectrum, `ring_hops` spectra.
    power: Vec<f32>,
    /// Bit log-likelihood ratios, `offsets * bits` per spectrum.
    llr: Vec<f32>,
    hops: u64,
    noise: f32,
    /// Bias of the noise estimate (the mean of the quietest quarter of the
    /// bins, against the mean of all of them in noise).
    noise_bias: f32,
    /// Smoothed scores, `offsets` per alignment within a block.
    score: Vec<f32>,
    reading: Vec<Reading>,
    /// Bit to tone tables: for each bit, the tones where it is 0 and 1.
    zero_tones: Vec<Vec<usize>>,
    one_tones: Vec<Vec<usize>>,
    // Output.
    /// Samples (at 8 kHz) taken in.
    samples: u64,
    /// Blocks judged good, waiting to be printed.
    blocks: Vec<Block>,
    /// When recent good blocks were judged, how well they fit, and where.
    fits: Vec<(u64, f32, f32)>,
    lines: Lines,
    last_char: u64,
    last_print: Option<u64>,
    chars: u64,
    found_center: Option<f32>,
    snr_db: Option<f32>,
    quality: f32,
    score_min: f32,
    tmp: Vec<f32>,
    sorted: Vec<f32>,
}

impl Chain {
    fn new(cfg: OliviaConfig) -> Self {
        let cfg = cfg.normalized();
        let family = cfg.family;
        let block = family.block();
        let block_hops = block * PHASES;
        let ring_hops = 2 * block_hops;
        let tones = cfg.tones as usize;
        let bits = cfg.bits();
        let n = cfg.symbol_len();
        let size = n * PAD;
        let fft = FftPlanner::new().plan_fft_forward(size);
        let scratch = vec![Complex32::new(0.0, 0.0); fft.get_inplace_scratch_len()];
        let win: Vec<f32> = (0..n)
            .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / n as f32).cos())
            .collect();
        let bin_hz = RATE / size as f32;
        let spacing_bins = 2 * PAD;
        let search_hz = SEARCH_HZ;
        let tone0_hz = cfg.center_hz - cfg.bandwidth_hz / 2.0 + cfg.baud() / 2.0;
        let tone0 = (tone0_hz / bin_hz).round() as i64;
        let top = tone0 + (spacing_bins * (tones - 1)) as i64;
        let max_bin = (size / 2) as i64 - 1;
        let mut reach = (search_hz / bin_hz).round() as i64;
        reach = reach.min(tone0 - 1).min(max_bin - top).max(0);
        let lo_bin = (tone0 - reach) as usize;
        let offsets = 2 * reach as usize + 1;
        let width = spacing_bins * (tones - 1) + offsets;

        let mut zero_tones = vec![Vec::new(); bits];
        let mut one_tones = vec![Vec::new(); bits];
        for k in 0..tones {
            let sym = ungray(k);
            for b in 0..bits {
                if (sym >> b) & 1 == 0 {
                    zero_tones[b].push(k)
                } else {
                    one_tones[b].push(k)
                }
            }
        }

        // Expected mean of the smallest quarter of `width` exponential
        // variables, against their mean.
        let m = (width / 4).max(1);
        let mut bias = 0.0;
        let mut acc = 0.0;
        for i in 1..=m {
            acc += 1.0 / (width - i + 1) as f32;
            bias += acc;
        }
        let noise_bias = bias / m as f32;

        Self {
            cfg,
            family,
            block,
            block_hops,
            ring_hops,
            tones,
            bits,
            n,
            hop: n / (2 * PHASES),
            fft,
            win,
            buf: vec![Complex32::new(0.0, 0.0); size],
            scratch,
            ring: vec![0.0; n],
            pos: 0,
            since_hop: 0,
            lo_bin,
            offsets,
            width,
            bin_hz,
            power: vec![0.0; ring_hops * width],
            llr: vec![0.0; ring_hops * offsets * bits],
            hops: 0,
            noise: 0.0,
            noise_bias,
            score: vec![0.0; block_hops * offsets],
            reading: vec![Reading::default(); block_hops * offsets],
            zero_tones,
            one_tones,
            samples: 0,
            blocks: Vec::new(),
            fits: Vec::new(),
            lines: Lines::default(),
            last_char: 0,
            last_print: None,
            chars: 0,
            found_center: None,
            snr_db: None,
            quality: 0.0,
            score_min: SCORE_MIN * (3.0 / bits as f32).sqrt() * family.score_scale(),
            tmp: vec![0.0; block],
            sorted: Vec::with_capacity(width),
        }
    }

    fn locked(&self) -> bool {
        let block = (self.block_hops * self.hop) as u64;
        self.last_print
            .is_some_and(|t| self.samples.saturating_sub(t) <= LOCK_HOLD_BLOCKS * block + block / 2)
    }

    fn push(&mut self, x: &[f32]) {
        for &v in x {
            self.ring[self.pos] = v;
            self.pos = (self.pos + 1) % self.n;
            self.samples += 1;
            self.since_hop += 1;
            if self.since_hop == self.hop {
                self.since_hop = 0;
                self.spectrum();
                self.hops += 1;
                self.read_blocks();
                self.judge();
            }
        }
    }

    /// One spectrum: tone powers, the noise floor, and each offset's bit
    /// likelihoods for a symbol ending here.
    fn spectrum(&mut self) {
        let size = self.buf.len();
        for i in 0..self.n {
            let s = self.ring[(self.pos + i) % self.n];
            self.buf[i] = Complex32::new(s * self.win[i], 0.0);
        }
        for z in self.buf[self.n..size].iter_mut() {
            *z = Complex32::new(0.0, 0.0);
        }
        self.fft
            .process_with_scratch(&mut self.buf, &mut self.scratch);
        let row = (self.hops as usize % self.ring_hops) * self.width;
        for j in 0..self.width {
            self.power[row + j] = self.buf[self.lo_bin + j].norm_sqr();
        }

        // Noise: the quietest quarter of the bins, corrected for picking
        // the quietest. The signal covers at most a few tones' worth of
        // bins at any instant, so a quarter is always between them.
        self.sorted.clear();
        self.sorted
            .extend_from_slice(&self.power[row..row + self.width]);
        let m = (self.width / 4).max(1);
        self.sorted
            .select_nth_unstable_by(m - 1, |a, b| a.total_cmp(b));
        let low = self.sorted[..m].iter().sum::<f32>() / m as f32 / self.noise_bias;
        if self.hops < 4 || self.noise <= 0.0 {
            self.noise = low;
        } else {
            self.noise += (low - self.noise) / NOISE_TAU_HOPS;
        }
        let inv = 1.0 / self.noise.max(1e-20);

        let lrow = (self.hops as usize % self.ring_hops) * self.offsets * self.bits;
        let step = 2 * PAD;
        let mut e = [0f32; 64];
        for o in 0..self.offsets {
            for (k, ek) in e.iter_mut().enumerate().take(self.tones) {
                *ek = self.power[row + o + step * k] * inv;
            }
            for b in 0..self.bits {
                let l0 = log_sum_exp(self.zero_tones[b].iter().map(|&k| e[k]));
                let l1 = log_sum_exp(self.one_tones[b].iter().map(|&k| e[k]));
                self.llr[lrow + o * self.bits + b] = (l0 - l1).clamp(-LLR_MAX, LLR_MAX);
            }
        }
    }

    /// Read the block of symbols ending at this spectrum as a block, at every
    /// offset, and fold each reading into the smoothed scores.
    fn read_blocks(&mut self) {
        let h = self.hops - 1;
        if h + 1 < self.block_hops as u64 {
            return;
        }
        let slot = h as usize % self.block_hops;
        for o in 0..self.offsets {
            let r = self.read(h, o, false);
            let i = slot * self.offsets + o;
            self.score[i] = r.score + SCORE_MEMORY * self.score[i];
            self.reading[i] = r;
        }
    }

    /// Read the block ending at spectrum `h` at offset `o`. Each character's
    /// log-likelihood is half its correlation with the received bit
    /// likelihoods. The odds a character is right are, when `exact`, taken
    /// against all 127 others; otherwise against the runner-up alone (the
    /// usual max-log shortcut, a little optimistic in noise, which the
    /// thresholds allow for), which spares 128 exponentials for each of the
    /// thousands of readings a second that are only there to find the
    /// alignment.
    fn read(&mut self, h: u64, o: usize, exact: bool) -> Reading {
        let mut r = Reading::default();
        let mut total = 0.0;
        let mut agree = 0.0;
        let stride = self.offsets * self.bits;
        for c in 0..self.bits {
            let mut mag = 0.0;
            for t in 0..self.block {
                let hop = h - ((self.block - 1 - t) * PHASES) as u64;
                let v = self.llr
                    [(hop as usize % self.ring_hops) * stride + o * self.bits + (c + t) % self.bits];
                mag += v.abs();
                self.tmp[t] = if scrambled(self.family, c, t) { -v } else { v };
            }
            fht(&mut self.tmp);
            let (mut best, mut second, mut at) = (0.0f32, 0.0f32, 0usize);
            for (j, v) in self.tmp.iter().enumerate() {
                let a = v.abs();
                if a > best {
                    second = best;
                    best = a;
                    at = j;
                } else if a > second {
                    second = a;
                }
            }
            let top = 0.5 * (second - best);
            let lo = if exact {
                // Summed in the log domain from the runner-up, so a clean
                // signal's odds keep growing instead of running out of
                // precision.
                let mut others = 0.0f32;
                for (j, v) in self.tmp.iter().enumerate() {
                    if j != at {
                        others += (0.5 * (v.abs() - best) - top).exp();
                    }
                    others += (0.5 * (-v.abs() - best) - top).exp();
                }
                -(top + others.ln())
            } else {
                -top
            };
            r.chars[c] = if self.tmp[at] >= 0.0 {
                at as u8
            } else {
                (at + self.block) as u8
            };
            r.log_odds[c] = lo;
            total += lo.max(-2.0);
            agree += best / mag.max(1e-9);
        }
        r.score = total / self.bits as f32;
        r.agreement = agree / self.bits as f32;
        r
    }

    /// Half a block after a reading, keep it if it is the surest reading
    /// of all alignments and offsets.
    fn judge(&mut self) {
        let h = self.hops - 1;
        if h + 1 < (self.block_hops + self.block_hops / 2) as u64 {
            return;
        }
        let end = h - (self.block_hops / 2) as u64;
        let slot = end as usize % self.block_hops;
        let (mut best, mut at) = (f32::NEG_INFINITY, 0);
        for (i, s) in self.score.iter().enumerate() {
            if *s > best {
                best = *s;
                at = i;
            }
        }
        if at / self.offsets != slot || best < self.score_min {
            return;
        }
        let o = at % self.offsets;
        if self.reading[at].agreement < self.family.agreement_min() {
            return;
        }
        let r = self.read(end, o, true);
        let syms = encode_block(self.family, &r.chars[..self.bits], self.bits);
        let p = |o: usize| self.sent_power(end, o, &syms);
        let mid = p(o);
        // The center between bins, from the power of the sent tones one bin
        // either side (a parabola through the three).
        let frac = if o > 0 && o + 1 < self.offsets {
            let (l, r) = (p(o - 1), p(o + 1));
            let den = l - 2.0 * mid + r;
            if den < 0.0 {
                (0.5 * (l - r) / den).clamp(-0.5, 0.5)
            } else {
                0.0
            }
        } else {
            0.0
        };
        let center =
            self.cfg.center_hz + (o as f32 + frac - (self.offsets / 2) as f32) * self.bin_hz;
        let snr = self.snr_from(mid);
        let chars = (0..self.bits)
            .filter(|&c| r.log_odds[c] >= CHAR_LOG_ODDS_MIN)
            .filter_map(|c| self.family.to_char(r.chars[c]))
            .collect();
        self.fits.push((self.samples, r.agreement, center));
        let keep = self.samples.saturating_sub((2.0 * HOLD_S * RATE) as u64);
        self.fits.retain(|f| f.0 >= keep);
        self.blocks.push(Block {
            t: self.samples,
            chars,
            agreement: r.agreement,
            quality: best,
            center,
            snr,
        });
    }

    /// Mean power on the tones a block's characters say were sent, at one
    /// offset.
    fn sent_power(&self, end: u64, o: usize, syms: &[usize]) -> f32 {
        let mut s = 0.0;
        for (t, sym) in syms.iter().enumerate() {
            let hop = end - ((self.block - 1 - t) * PHASES) as u64;
            let row = (hop as usize % self.ring_hops) * self.width;
            s += self.power[row + o + 2 * PAD * gray(*sym)];
        }
        s / self.block as f32
    }

    /// Signal to noise in 2500 Hz from the mean power on the sent tones.
    fn snr_from(&self, sent: f32) -> f32 {
        let s = (sent - self.noise).max(1e-20);
        // A sine of amplitude A puts (A/2 sum w)^2 in its bin; noise of
        // variance v puts v sum w^2 in each. With a Hann window that makes
        // the SNR in 2500 Hz at 8 kHz 4.8 S / (N noise). An Olivia symbol's
        // power is spread over its two-symbol window, partly where the
        // analysis window is low, so it shows less than a steady tone of
        // the same mean power would: the factor is measured in the tests.
        (10.0 * (SNR_FACTOR * s / (self.n as f32 * self.noise)).log10()).min(60.0)
    }

    /// Print a block's characters.
    fn print(&mut self, b: Block, events: &mut Vec<OliviaEvent>) {
        self.last_print = Some(b.t);
        self.found_center = Some(b.center);
        self.snr_db = Some(b.snr);
        self.quality = b.quality;
        for ch in b.chars {
            if ch != '\n' {
                self.chars += 1;
            }
            self.last_char = b.t;
            if let Some(line) = self.lines.push(ch) {
                self.emit(line, events);
            }
        }
    }

    fn flush(&mut self, events: &mut Vec<OliviaEvent>) {
        if let Some(line) = self.lines.take() {
            self.emit(line, events);
        }
    }

    fn emit(&mut self, text: String, events: &mut Vec<OliviaEvent>) {
        events.push(OliviaEvent::Text {
            text,
            tones: self.cfg.tones,
            bandwidth_hz: self.cfg.bandwidth_hz as u32,
            center_hz: self.found_center.unwrap_or(self.cfg.center_hz),
            snr_db: self.snr_db.unwrap_or(f32::NEG_INFINITY),
        });
    }
}

/// A block judged good, not yet printed.
struct Block {
    /// When it was judged, in samples at 8 kHz.
    t: u64,
    chars: Vec<char>,
    agreement: f32,
    quality: f32,
    center: f32,
    snr: f32,
}

fn log_sum_exp(it: impl Iterator<Item = f32> + Clone) -> f32 {
    let m = it.clone().fold(f32::NEG_INFINITY, f32::max);
    m + it.map(|v| (v - m).exp()).sum::<f32>().ln()
}

// ---------------------------------------------------------------------------
// The decoder.

/// Olivia receiver for one setting, or several side by side.
pub struct OliviaDecoder {
    resampler: Resampler,
    chains: Vec<Chain>,
    /// How long a block waits, in samples, for another setting's receiver
    /// to show it fits the signal better.
    hold: u64,
    tmp: Vec<f32>,
    /// Static crashes zeroed before any receiver sees them (the RTTY
    /// decoder's blanker, told where the signal is once one is locked).
    blanker: crate::rtty::Blanker,
    blanked: Vec<f32>,
    events: Vec<OliviaEvent>,
}

impl OliviaDecoder {
    /// One setting.
    pub fn new(fs: f32, cfg: OliviaConfig) -> Self {
        Self::with_configs(fs, &[cfg])
    }

    /// The common settings ([`OliviaConfig::common`]) at one center, side by
    /// side: whichever the signal is, its receiver prints it.
    pub fn new_auto(fs: f32, center_hz: f32) -> Self {
        Self::with_configs(fs, &OliviaConfig::common(center_hz))
    }

    /// Contestia's common settings, side by side.
    pub fn new_contestia_auto(fs: f32, center_hz: f32) -> Self {
        Self::with_configs(fs, &OliviaConfig::common_contestia(center_hz))
    }

    /// Several settings side by side. A strong signal of one setting can
    /// look like text to the receiver of another (an 8/250 to a 4/250, whose
    /// tones and bits are a subset of it), so what a receiver reads waits
    /// [`HOLD_S`] and is dropped if another fit the signal clearly better
    /// meanwhile.
    pub fn with_configs(fs: f32, cfgs: &[OliviaConfig]) -> Self {
        let cfgs = if cfgs.is_empty() {
            vec![OliviaConfig::default()]
        } else {
            cfgs.to_vec()
        };
        let hold = if cfgs.len() > 1 {
            (HOLD_S * RATE) as u64
        } else {
            0
        };
        Self {
            resampler: Resampler::new(fs, RATE, 3000.0, 5000.0),
            chains: cfgs.into_iter().map(Chain::new).collect(),
            hold,
            tmp: Vec::with_capacity(4096),
            blanker: crate::rtty::Blanker::new(RATE),
            blanked: Vec::with_capacity(4096),
            events: Vec::new(),
        }
    }

    pub fn push(&mut self, audio: &[f32]) {
        self.tmp.clear();
        self.resampler.process(audio, &mut self.tmp);
        self.blanked.clear();
        for &x in &self.tmp {
            if let Some((y, _)) = self.blanker.push(x) {
                self.blanked.push(y);
            }
        }
        for c in self.chains.iter_mut() {
            c.push(&self.blanked);
        }
        let band = self.chains.iter().find(|c| c.locked()).map(|c| {
            let center = c.found_center.unwrap_or(c.cfg.center_hz);
            (center - c.cfg.bandwidth_hz / 2.0, center + c.cfg.bandwidth_hz / 2.0)
        });
        self.blanker.set_pair(band);
        let now = self.chains[0].samples;
        self.release(now.saturating_sub(self.hold));
        for c in self.chains.iter_mut() {
            if !c.lines.is_empty() && now.saturating_sub(c.last_char) as f32 > FLUSH_S * RATE {
                c.flush(&mut self.events);
            }
        }
    }

    /// Print the blocks judged before `until` that no other receiver
    /// outdid.
    fn release(&mut self, until: u64) {
        for i in 0..self.chains.len() {
            let ready: Vec<Block> = {
                let c = &mut self.chains[i];
                let n = c.blocks.iter().take_while(|b| b.t <= until).count();
                c.blocks.drain(..n).collect()
            };
            for b in ready {
                let hold = self.hold;
                let beaten = self.chains.iter().enumerate().any(|(j, d)| {
                    j != i
                        && d.fits
                            .iter()
                            .any(|&(t, a, _)| t.abs_diff(b.t) <= hold && a > b.agreement + BETTER_FIT)
                });
                // Interference (FT8 in the passband, say) makes an isolated
                // confident block now and then; a transmission makes a run
                // of them on one frequency.
                let c = &self.chains[i];
                let span = 3 * (c.block_hops * c.hop) as u64;
                let alone = hold > 0
                    && !c.fits.iter().any(|&(t, _, f)| t != b.t && t.abs_diff(b.t) <= span && (f - b.center).abs() <= CORROBORATE_HZ);
                if !beaten && !alone {
                    self.chains[i].print(b, &mut self.events);
                }
            }
        }
    }

    pub fn take(&mut self) -> Vec<OliviaEvent> {
        std::mem::take(&mut self.events)
    }

    /// Print whatever text is pending.
    pub fn flush(&mut self) {
        self.release(u64::MAX);
        for c in self.chains.iter_mut() {
            c.flush(&mut self.events);
        }
    }

    pub fn status(&self) -> OliviaStatus {
        // The receiver that printed last, else the first.
        let c = self
            .chains
            .iter()
            .filter(|c| c.last_print.is_some())
            .max_by_key(|c| c.last_print)
            .unwrap_or(&self.chains[0]);
        OliviaStatus {
            locked: c.locked(),
            center_hz: c.found_center.unwrap_or(c.cfg.center_hz),
            snr_db: c.snr_db,
            tones: c.cfg.tones,
            bandwidth_hz: c.cfg.bandwidth_hz as u32,
            chars: self.chains.iter().map(|c| c.chars).sum(),
            quality: c.quality,
        }
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtty::testsig::{Rng, add_noise, chunks, levenshtein, rayleigh};

    const BLOCK: usize = 64;

    /// Text as Contestia's transmitter codes it (fldigi's table).
    fn contestia_code(b: u8) -> u8 {
        let b = b.to_ascii_uppercase();
        match b {
            b' ' => 59,
            b'\r' => 60,
            b'\n' | 0 => 0,
            33..=90 => b - 32,
            8 => 61,
            _ => b'?' - 32,
        }
    }

    /// An Olivia transmitter as Jalocha's modulator builds it: raised-cosine
    /// symbols two symbol periods long, overlapping by half, each with a
    /// random quarter-cycle phase step. `ppm` stretches its clock. Mean
    /// power is that of a sine of amplitude `amp`.
    fn olivia_tx(
        text: &str,
        cfg: OliviaConfig,
        fs: f32,
        ppm: f64,
        offset_hz: f32,
        amp: f32,
        rng: &mut Rng,
    ) -> Vec<f32> {
        let cfg = cfg.normalized();
        let bits = cfg.bits();
        let mut bytes: Vec<u8> = match cfg.family {
            Family::Olivia => text.bytes().collect(),
            Family::Contestia => text.bytes().map(contestia_code).collect(),
        };
        while !bytes.len().is_multiple_of(bits) {
            bytes.push(0);
        }
        let mut syms = Vec::new();
        for blk in bytes.chunks(bits) {
            syms.extend(encode_block(cfg.family, blk, bits).iter().map(|s| gray(*s)));
        }
        let scale = 1.0 + ppm * 1e-6;
        let len_s = cfg.symbol_len() as f64 / RATE as f64 * scale;
        let sep_s = len_s / 2.0;
        let lead = (0.5 * fs as f64) as usize;
        let tail = (2.0 * fs as f64) as usize;
        let total = lead + ((syms.len() as f64 + 1.0) * sep_s * fs as f64) as usize + tail;
        let mut out = vec![0.0f32; total];
        let spacing = cfg.baud() as f64;
        let f0 = (cfg.center_hz + offset_hz) as f64 - cfg.bandwidth_hz as f64 / 2.0 + spacing / 2.0;
        let mut phase = 0.0f64;
        for (m, &k) in syms.iter().enumerate() {
            let t0 = lead as f64 / fs as f64 + m as f64 * sep_s;
            let f = f0 + spacing * k as f64;
            phase += if rng.uniform() < 0.5 { 0.25 } else { -0.25 } * std::f64::consts::TAU;
            let i0 = (t0 * fs as f64).ceil() as usize;
            let i1 = ((t0 + len_s) * fs as f64).floor() as usize;
            for (i, v) in out.iter_mut().enumerate().take(i1.min(total)).skip(i0) {
                let t = i as f64 / fs as f64;
                let u = (t - t0) / len_s;
                let w = 1.0 - (std::f64::consts::TAU * u).cos();
                *v += (w * (std::f64::consts::TAU * f * (t - t0) + phase).cos()) as f32;
            }
        }
        // Normalize to the mean power of the keyed part.
        let a = lead;
        let b = total - tail;
        let p = out[a..b].iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / (b - a) as f64;
        let g = (0.5 * amp as f64 * amp as f64 / p).sqrt() as f32;
        out.iter_mut().for_each(|v| *v *= g);
        out
    }

    const TEXT: &str =
        "CQ CQ CQ DE W1AW W1AW PSE K. THE QUICK BROWN FOX JUMPS OVER THE LAZY DOG 0123456789 ";

    fn run(dec: &mut OliviaDecoder, x: &[f32]) -> String {
        chunks(x, |c| dec.push(c));
        dec.flush();
        let mut s = String::new();
        for e in dec.take() {
            let OliviaEvent::Text { text, .. } = e;
            if !s.is_empty() {
                s.push(' ');
            }
            s.push_str(&text);
        }
        s
    }

    /// Character error rate, blanks aside: lines are cut at 80 characters,
    /// and the cut is not a character error.
    fn cer(sent: &str, got: &str) -> f32 {
        let a: String = sent.split_whitespace().collect();
        let b: String = got.split_whitespace().collect();
        levenshtein(&a, &b) as f32 / a.chars().count() as f32
    }

    #[test]
    fn hadamard_round_trip() {
        for ch in 0..128u8 {
            let syms = encode_block(Family::Olivia, &[ch, 0, 0], 3);
            let mut v = [0f32; BLOCK];
            for (t, s) in syms.iter().enumerate() {
                let bit = (s >> (t % 3)) & 1;
                let l = if bit == 0 { 1.0 } else { -1.0 };
                v[t] = if scrambled(Family::Olivia, 0, t) { -l } else { l };
            }
            fht(&mut v);
            let (at, best) = v
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
                .unwrap();
            let got = if *best >= 0.0 {
                at as u8
            } else {
                at as u8 + 64
            };
            assert_eq!(got, ch);
            assert!((best.abs() - 64.0).abs() < 1e-3);
        }
        // The transform pair agrees with Jalocha's: inverse then forward is
        // 64 times the identity.
        let mut v = [0f32; BLOCK];
        v[5] = 1.0;
        ifht(&mut v);
        fht(&mut v);
        assert!(
            (v[5] - 64.0).abs() < 1e-3
                && v.iter().enumerate().all(|(i, x)| i == 5 || x.abs() < 1e-3)
        );
    }

    #[test]
    fn symbol_lengths_match_the_modes() {
        let n = |t, b| OliviaConfig::new(t, b, 1500.0).symbol_len();
        assert_eq!(n(8, 500.0), 256);
        assert_eq!(n(32, 1000.0), 512);
        assert_eq!(n(8, 250.0), 512);
        assert_eq!(n(16, 500.0), 512);
        assert_eq!(n(4, 125.0), 512);
        assert_eq!(n(64, 2000.0), 512);
        assert!((OliviaConfig::new(32, 1000.0, 0.0).baud() - 31.25).abs() < 1e-6);
    }

    #[test]
    fn clean_copy_every_common_setting() {
        let fs = 47_904.0;
        for cfg in OliviaConfig::common(1500.0) {
            let mut rng = Rng::new(7);
            let x = olivia_tx(TEXT, cfg, fs, 0.0, 0.0, 0.3, &mut rng);
            let mut dec = OliviaDecoder::new(fs, cfg);
            let got = run(&mut dec, &x);
            assert!(cer(TEXT, &got) < 0.02, "{}: {got:?}", cfg.label());
        }
    }

    #[test]
    fn offset_and_clock_error() {
        let fs = 47_904.0;
        let cfg = OliviaConfig::default();
        for (off, ppm) in [(50.0, 100.0), (-50.0, -100.0), (23.0, 0.0)] {
            let mut rng = Rng::new(11);
            let mut x = olivia_tx(TEXT, cfg, fs, ppm, off, 0.3, &mut rng);
            add_noise(&mut x, 0.3, 0.0, fs, &mut rng);
            let mut dec = OliviaDecoder::new(fs, cfg);
            let got = run(&mut dec, &x);
            assert!(cer(TEXT, &got) < 0.02, "{off} Hz {ppm} ppm: {got:?}");
            let st = dec.status();
            assert!((st.center_hz - 1500.0 - off).abs() < 10.0, "{st:?}");
        }
    }

    #[test]
    fn auto_finds_the_setting() {
        let fs = 47_904.0;
        let cfg = OliviaConfig::new(16, 500.0, 1500.0);
        let mut rng = Rng::new(3);
        let mut x = olivia_tx(TEXT, cfg, fs, 30.0, 20.0, 0.3, &mut rng);
        add_noise(&mut x, 0.3, -3.0, fs, &mut rng);
        let mut dec = OliviaDecoder::new_auto(fs, 1500.0);
        let mut all = Vec::new();
        chunks(&x, |c| dec.push(c));
        dec.flush();
        for e in dec.take() {
            all.push(e);
        }
        let text: Vec<String> = all
            .iter()
            .map(|e| {
                let OliviaEvent::Text {
                    text,
                    tones,
                    bandwidth_hz,
                    ..
                } = e;
                assert_eq!((*tones, *bandwidth_hz), (16, 500), "{e:?}");
                text.clone()
            })
            .collect();
        assert!(cer(TEXT, &text.join(" ")) < 0.02, "{text:?}");
        assert_eq!(dec.status().tones, 16);
    }

    fn sweep(cfg: OliviaConfig, fading: bool, snrs: &[f32]) -> Vec<(f32, f32)> {
        let fs = 47_904.0;
        let long: String = TEXT.repeat(2);
        snrs.iter()
            .map(|&snr| {
                let mut worst = 0.0f32;
                for seed in 1..=3u64 {
                    let mut rng = Rng::new(seed * 101);
                    let mut x = olivia_tx(&long, cfg, fs, 60.0, 30.0, 0.3, &mut rng);
                    if fading {
                        // Rayleigh fading on the whole signal, 1 Hz spread.
                        let g = rayleigh(x.len(), fs, 1.0, &mut rng);
                        for (v, g) in x.iter_mut().zip(g) {
                            *v *= g.norm();
                        }
                    }
                    add_noise(&mut x, 0.3, snr, fs, &mut rng);
                    let mut dec = OliviaDecoder::new(fs, cfg);
                    let got = run(&mut dec, &x);
                    worst = worst.max(cer(&long, &got));
                }
                (snr, worst)
            })
            .collect()
    }

    /// Character errors under static crashes (two and five a second, 20 to
    /// 35 dB over the signal), with the blanker and, under
    /// `USDR_NO_BLANK`, without:
    /// `cargo test --release -p scannerd-engine olivia::tests::impulse_sweep -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn impulse_sweep() {
        let fs = 47_904.0;
        let long: String = TEXT.repeat(2);
        for cfg in [OliviaConfig::default(), OliviaConfig::new(16, 500.0, 1500.0), OliviaConfig::new(32, 1000.0, 1500.0)] {
            for snr in [0.0f32, -6.0, -10.0] {
                for rate in [0.0f32, 2.0, 5.0] {
                    let mut total = 0.0;
                    for seed in 1..=3u64 {
                        let mut rng = Rng::new(seed * 101);
                        let mut x = olivia_tx(&long, cfg, fs, 60.0, 30.0, 0.3, &mut rng);
                        add_noise(&mut x, 0.3, snr, fs, &mut rng);
                        if rate > 0.0 {
                            crate::rtty::testsig::add_impulses(&mut x, 0.3, fs, rate, &mut rng);
                        }
                        let mut dec = OliviaDecoder::new(fs, cfg);
                        total += cer(&long, &run(&mut dec, &x)).min(1.0);
                    }
                    eprintln!("{} {snr:+} dB, {rate} crashes/s: CER {:.3}", cfg.label(), total / 3.0);
                }
            }
        }
    }

    /// The SNR at which 8/500 copies with under 2% character errors.
    /// Run with `--ignored --nocapture` to see the sweep.
    #[test]
    #[ignore]
    fn sensitivity_sweep() {
        for (cfg, fading) in [
            (OliviaConfig::default(), false),
            (OliviaConfig::default(), true),
            (OliviaConfig::new(8, 250.0, 1500.0), false),
            (OliviaConfig::new(32, 1000.0, 1500.0), false),
            (OliviaConfig::new(16, 500.0, 1500.0), false),
        ] {
            let snrs: &[f32] = if fading {
                &[-2.0, -4.0, -6.0, -8.0, -10.0, -12.0]
            } else {
                &[-8.0, -10.0, -11.0, -12.0, -13.0, -14.0, -15.0]
            };
            let r = sweep(cfg, fading, snrs);
            eprintln!(
                "{} {}: {:?}",
                cfg.label(),
                if fading { "fading" } else { "AWGN" },
                r
            );
        }
    }

    /// The SNR reading against the SNR the test set (`--nocapture` to see
    /// it).
    #[test]
    fn snr_reading() {
        let fs = 47_904.0;
        for cfg in [
            OliviaConfig::default(),
            OliviaConfig::new(32, 1000.0, 1500.0),
        ] {
            for snr in [-8.0f32, 0.0, 10.0] {
                let mut rng = Rng::new(21);
                let mut x = olivia_tx(TEXT, cfg, fs, 0.0, 0.0, 0.3, &mut rng);
                add_noise(&mut x, 0.3, snr, fs, &mut rng);
                let mut dec = OliviaDecoder::new(fs, cfg);
                chunks(&x, |c| dec.push(c));
                let got = dec.status().snr_db.expect("snr");
                eprintln!("{} at {snr} dB reads {got:.1}", cfg.label());
                assert!(
                    (got - snr).abs() < 2.0,
                    "{} at {snr} dB reads {got}",
                    cfg.label()
                );
            }
        }
    }

    #[test]
    fn copies_at_minus_11_db() {
        let r = sweep(OliviaConfig::default(), false, &[-11.0]);
        assert!(r[0].1 < 0.02, "{r:?}");
    }

    /// A strong signal of each common setting, heard by the automatic
    /// decoder: only its own setting prints.
    #[test]
    fn other_settings_stay_quiet() {
        let fs = 47_904.0;
        for cfg in OliviaConfig::common(1500.0) {
            for snr in [30.0] {
                let mut rng = Rng::new(5);
                let mut x = olivia_tx(TEXT, cfg, fs, 0.0, 10.0, 0.3, &mut rng);
                add_noise(&mut x, 0.3, snr, fs, &mut rng);
                let mut dec = OliviaDecoder::new_auto(fs, 1500.0);
                chunks(&x, |c| dec.push(c));
                dec.flush();
                for e in dec.take() {
                    let OliviaEvent::Text {
                        tones,
                        bandwidth_hz,
                        ..
                    } = &e;
                    assert_eq!(
                        (*tones, *bandwidth_hz),
                        (cfg.tones, cfg.bandwidth_hz as u32),
                        "{} at {snr} dB: {e:?}",
                        cfg.label()
                    );
                }
            }
        }
    }

    /// Seconds of audio decoded per second of work. `--ignored
    /// --nocapture`.
    #[test]
    #[ignore]
    fn speed() {
        let fs = 47_904.0;
        let mut rng = Rng::new(1);
        let mut x = olivia_tx(TEXT, OliviaConfig::default(), fs, 0.0, 0.0, 0.3, &mut rng);
        add_noise(&mut x, 0.3, 0.0, fs, &mut rng);
        let secs = x.len() as f64 / fs as f64;
        for auto in [false, true] {
            let mut dec = if auto {
                OliviaDecoder::new_auto(fs, 1500.0)
            } else {
                OliviaDecoder::new(fs, OliviaConfig::default())
            };
            let t = std::time::Instant::now();
            chunks(&x, |c| dec.push(c));
            eprintln!(
                "{}: {:.0}x real time",
                if auto { "auto (6 settings)" } else { "8/500" },
                secs / t.elapsed().as_secs_f64()
            );
        }
    }

    #[test]
    fn noise_prints_nothing() {
        let fs = 47_904.0;
        let mut rng = Rng::new(99);
        // A minute and a half of noise for each setting, at the level a
        // signal would have.
        let n = (90.0 * fs) as usize;
        let x: Vec<f32> = (0..n).map(|_| 0.3 * rng.gauss()).collect();
        let mut dec = OliviaDecoder::new_auto(fs, 1500.0);
        chunks(&x, |c| dec.push(c));
        dec.flush();
        let ev = dec.take();
        assert!(ev.is_empty(), "{ev:?}");
        assert!(!dec.status().locked);
    }

    #[test]
    fn contestia_hadamard_round_trip() {
        let f = Family::Contestia;
        for ch in 0..64u8 {
            let syms = encode_block(f, &[ch, 0, 0], 3);
            assert_eq!(syms.len(), 32);
            let mut v = [0f32; 32];
            for (t, s) in syms.iter().enumerate() {
                let bit = (s >> (t % 3)) & 1;
                let l = if bit == 0 { 1.0 } else { -1.0 };
                v[t] = if scrambled(f, 0, t) { -l } else { l };
            }
            fht(&mut v);
            let (at, best) = v.iter().enumerate().max_by(|a, b| a.1.abs().total_cmp(&b.1.abs())).unwrap();
            assert_eq!(if *best >= 0.0 { at as u8 } else { at as u8 + 32 }, ch);
        }
    }

    #[test]
    fn contestia_alphabet_reads_back() {
        for b in b"CQ DE W1AW 0123456789 ./?".iter().copied() {
            assert_eq!(Family::Contestia.to_char(contestia_code(b)), Some(b as char));
        }
        assert_eq!(Family::Contestia.to_char(contestia_code(b'q')), Some('Q'));
        assert_eq!(Family::Contestia.to_char(0), None);
        assert_eq!(Family::Contestia.to_char(61), None);
    }

    #[test]
    fn contestia_clean_copy_every_common_setting() {
        let fs = 47_904.0;
        for cfg in OliviaConfig::common_contestia(1500.0) {
            let mut rng = Rng::new(7);
            let x = olivia_tx(TEXT, cfg, fs, 0.0, 0.0, 0.3, &mut rng);
            let mut dec = OliviaDecoder::new(fs, cfg);
            let got = run(&mut dec, &x);
            assert!(cer(TEXT, &got) < 0.02, "{}: {got:?}", cfg.label());
        }
    }

    #[test]
    fn contestia_copies_off_frequency_in_noise() {
        let fs = 47_904.0;
        let cfg = OliviaConfig::contestia(8, 500.0, 1500.0);
        let mut rng = Rng::new(11);
        let mut x = olivia_tx(TEXT, cfg, fs, 50.0, 40.0, 0.3, &mut rng);
        add_noise(&mut x, 0.3, -6.0, fs, &mut rng);
        let mut dec = OliviaDecoder::new(fs, cfg);
        let got = run(&mut dec, &x);
        assert!(cer(TEXT, &got) < 0.05, "{got:?}");
    }

    #[test]
    fn contestia_auto_finds_the_setting() {
        let fs = 47_904.0;
        let cfg = OliviaConfig::contestia(16, 500.0, 1500.0);
        let mut rng = Rng::new(3);
        let mut x = olivia_tx(TEXT, cfg, fs, 30.0, 20.0, 0.3, &mut rng);
        add_noise(&mut x, 0.3, 0.0, fs, &mut rng);
        let mut dec = OliviaDecoder::new_contestia_auto(fs, 1500.0);
        chunks(&x, |c| dec.push(c));
        dec.flush();
        let mut got = String::new();
        for e in dec.take() {
            let OliviaEvent::Text { text, tones, bandwidth_hz, .. } = e;
            assert_eq!((tones, bandwidth_hz), (16, 500), "{text}");
            got.push_str(&text);
        }
        assert!(cer(TEXT, &got) < 0.05, "{got:?}");
    }

    /// The false-print floor, printed for setting the thresholds:
    /// `cargo test -p scannerd-engine --release contestia_noise_floor -- --ignored --nocapture`
    #[test]
    #[ignore = "ten minutes of noise per setting"]
    fn contestia_noise_floor() {
        let fs = 8_000.0;
        for cfg in OliviaConfig::common_contestia(1500.0) {
            let mut rng = Rng::new(21);
            let mut x = vec![0f32; (600.0 * fs) as usize];
            add_noise(&mut x, 0.3, 0.0, fs, &mut rng);
            let mut c = Chain::new(cfg);
            let (mut best, mut printed) = (f32::NEG_INFINITY, 0usize);
            for s in x.chunks(512) {
                c.push(s);
                let m = c.score.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                best = best.max(m);
                printed += c.blocks.len();
                c.blocks.clear();
            }
            println!("{}: best score {best:.1} against {:.1}, {printed} blocks printed", cfg.label(), c.score_min);
        }
    }
}
