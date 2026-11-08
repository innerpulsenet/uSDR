//! DominoEX and Thor: incremental frequency keying (IFK+).
//!
//! Both send 18 tones, one at a time, phase continuous and a symbol apart
//! in time. The data is in the step from one tone to the next: a nibble n
//! moves the tone up by n + 2, modulo 18, so a tone never repeats and never
//! moves up by one. That makes the receiver indifferent to where the tones
//! sit, and it is what makes a steady carrier impossible to mistake for one.
//!
//! The two differ only in the nibbles. DominoEX sends a varicode of one to
//! three nibbles directly. Thor sends MFSK varicode through the NASA K=7
//! rate 1/2 code and a 4 x 10 diagonal interleaver, four coded bits to a
//! symbol. (fldigi's 25x4, 50x1, 50x2 and 100 use a K=15 code and are not
//! decoded here.) The modes, code tables and framing follow fldigi; the
//! receiver below is built differently, for a band nobody has tuned.
//!
//! Searching: each speed takes a spectrum of one symbol's samples every
//! quarter symbol, bins half a baud apart, each bin divided by its own
//! clipped running level so a steady carrier flattens to nothing. At every
//! bin and timing quarter, the peak of the 18 comb points scores a symbol
//! if it stands alone and the step from the last peak is one IFK+ can make.
//! A run of those starts a tracker.
//!
//! Tracking: the tracker looks back over samples already kept to fix the
//! timing, frequency and where the 18 tones are, then reads from where the
//! signal began. Each symbol's 18 tones give a likelihood for each of the
//! 16 nibbles from every pair of tones either side of the step, so a weak
//! tone costs a soft bit rather than a wrong nibble.
//!
//! Printing: nothing is printed until a tracker has a run of symbols, of
//! characters that make sense and, for Thor, of coded bits that agree with
//! the Viterbi decoder's path. Printed characters wait a little so the last
//! symbols of a signal, which are noise, can be cut off. A line shorter than
//! two characters is not printed.
//!
//! Not decoded: the K=15 modes, Thor's images and either mode's secondary
//! text, and signals sent with the tones reversed (lower sideband).

mod tables;
#[cfg(test)]
mod tests;

use crate::mfsk::fec::{Encoder, Interleaver, Viterbi};
use crate::mfsk::resample::Resampler;
use crate::mfsk::varicode;
use crate::rtty::Lines;
use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::{Arc, OnceLock};
use tables::{DOMINO_CODES, THOR_SECONDARY};

/// Tones either mode sends.
const TONES: usize = 18;
/// Extra comb points each side of the 18 while tracking, so a tone a little
/// outside where the tones were reckoned to be is still read.
const WING: usize = 6;
const WIN: usize = TONES + 2 * WING;
/// The band searched: where a signal's center may be, in audio Hz.
const SEARCH_LO: f32 = 500.0;
const SEARCH_HI: f32 = 2500.0;
/// Spectra per symbol while searching (timing is tried at quarter symbols).
const PHASES: usize = 4;
/// Symbols of samples kept.
const RING_SYMBOLS: usize = 72;
/// How far back a new tracker may start.
const RETRO_SYMBOLS: usize = 64;
/// Symbols used to fix a new tracker's timing and frequency.
const REFINE_SYMBOLS: usize = 24;
/// A bin's running level climbs by at most this factor of itself at once,
/// so a tone that comes and goes hardly raises it and a carrier does.
const LEVEL_CLIP: f32 = 3.0;
const SEARCH_LEVEL_TAU_HOPS: f32 = 64.0;
const TRACK_LEVEL_TAU_SYMBOLS: f32 = 24.0;
/// Log-likelihood ratios of coded bits are limited to this.
const LLR_MAX: f32 = 12.0;
/// A tone's normalized power counts for no more than this.
const LIKELIHOOD_CAP: f32 = 16.0;
/// Viterbi decision depth, as fldigi's traceback.
const VITERBI_DELAY: u32 = 45;

/// The search score's memory, in symbols.
const SCORE_TAU: f32 = 24.0;
/// A symbol's score: its peak (limited to `SCORE_PEAK_CAP`) less
/// `SCORE_PEAK_BIAS`, when it passes the tests; else `SCORE_BAD`.
const SCORE_PEAK_CAP: f32 = 12.0;
const SCORE_PEAK_BIAS: f32 = 5.0;
const SCORE_BAD: f32 = -3.0;
/// The peak must be this many times the runner-up.
const SCORE_PURITY: f32 = 2.2;
/// A tracker starts at this score.
const SCORE_START: f32 = 32.0;
/// A symbol is "good" for a tracker with its peak over this much...
const GOOD_PEAK: f32 = 6.5;
/// ...and this much over the next tone.
const GOOD_PURITY: f32 = 1.8;
/// ...and this share of the tracker's running peak.
const GOOD_QUALITY: f32 = 0.3;
/// Power half a baud off the tone must be less than this share of the tone's.
const GRID_MARGIN: f32 = 1.2;
/// Symbols of "good" flags looked at.
const RATE_WINDOW: u32 = 24;
/// Of `RATE_WINDOW`: a tracker prints only with at least this many good...
const RATE_OPEN: u32 = 16;
/// ...is given up at the start below this...
const RATE_DROP: u32 = 8;
/// ...and once printing, is finished below this.
const RATE_END: u32 = 8;
/// Characters that must make sense before anything is printed.
const OPEN_CHARS: usize = 10;
/// A tracker that has not opened after this many symbols is given up.
const OPEN_TIMEOUT: u64 = 240;
/// Plain DominoEX symbols dropped from the end of a run: without a code to
/// check them the last few cannot be told from the noise after the signal,
/// and a sender ends with idles anyway.
const TAIL_TRIM: u64 = 9;
/// Symbols a character waits between being read and being printed.
const HOLD_SYMBOLS: u64 = 24;
/// Thor (and FEC DominoEX) opens only with the coded bits agreeing with the
/// Viterbi decoder's path this well (weighted by confidence).
const AGREE_OPEN: f32 = 0.86;
/// Plain DominoEX refuses to open when the coded bits agree this well: a
/// signal that fits the code is Thor, not DominoEX.
const AGREE_IS_THOR: f32 = 0.90;
/// Coded pairs agreement is judged from before anything opens. Plain
/// DominoEX waits for it too, since it is the only proof it is not Thor.
const AGREE_MIN_PAIRS: u32 = 40;
/// Agreement's memory, in coded pairs.
const AGREE_TAU: f32 = 64.0;
/// Pending text is let go after this long without a character.
const FLUSH_S: f32 = 8.0;
/// Most trackers on one speed at once.
const MAX_TRACKS: usize = 3;
/// After a tracker is dropped, its stretch of band is left alone this long
/// (in score memories).
const COOLDOWN_TAUS: u64 = 3;

/// Which of the two modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    DominoEx,
    DominoExFec,
    Thor,
}

/// The speeds both modes share (the names are fldigi's).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IfkSpeed {
    Micro,
    S4,
    S5,
    S8,
    S11,
    S16,
    S22,
}

impl IfkSpeed {
    pub const ALL: [IfkSpeed; 7] = [
        IfkSpeed::Micro,
        IfkSpeed::S4,
        IfkSpeed::S5,
        IfkSpeed::S8,
        IfkSpeed::S11,
        IfkSpeed::S16,
        IfkSpeed::S22,
    ];

    pub fn name(self) -> &'static str {
        match self {
            IfkSpeed::Micro => "MICRO",
            IfkSpeed::S4 => "4",
            IfkSpeed::S5 => "5",
            IfkSpeed::S8 => "8",
            IfkSpeed::S11 => "11",
            IfkSpeed::S16 => "16",
            IfkSpeed::S22 => "22",
        }
    }

    /// Samples per symbol, spacing in bins of a symbol's DFT, and the rate
    /// the mode is defined at.
    fn timing(self) -> (usize, usize, f32) {
        match self {
            IfkSpeed::Micro => (4000, 1, 8000.0),
            IfkSpeed::S4 => (2048, 2, 8000.0),
            IfkSpeed::S5 => (2048, 2, 11025.0),
            IfkSpeed::S8 => (1024, 2, 8000.0),
            IfkSpeed::S11 => (1024, 1, 11025.0),
            IfkSpeed::S16 => (512, 1, 8000.0),
            IfkSpeed::S22 => (512, 1, 11025.0),
        }
    }

    pub fn baud(self) -> f32 {
        let (s, _, r) = self.timing();
        r / s as f32
    }

    pub fn spacing_hz(self) -> f32 {
        let (_, ds, _) = self.timing();
        self.baud() * ds as f32
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum IfkEvent {
    /// Decoded text, a line at a time (or what there is after a pause).
    Text {
        text: String,
        /// "DOMEX11", "DOMEX11FEC", "THOR8" and so on.
        mode: String,
        center_hz: f32,
        snr_db: f32,
    },
}

impl IfkEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            IfkEvent::Text { .. } => "text",
        }
    }

    pub fn summary(&self) -> String {
        match self {
            IfkEvent::Text { text, mode, .. } => format!("{mode}: {text}"),
        }
    }

    pub fn fields(&self) -> Vec<(&'static str, String)> {
        match self {
            IfkEvent::Text {
                text,
                mode,
                center_hz,
                snr_db,
            } => vec![
                ("text", text.clone()),
                ("mode", mode.clone()),
                ("centerHz", format!("{center_hz:.1}")),
                ("snrDb", format!("{snr_db:.1}")),
            ],
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct IfkStatus {
    /// A signal is being printed.
    pub locked: bool,
    /// Where the printing signal's center is, or the configured center.
    pub center_hz: f32,
    /// Signal to noise in 2500 Hz of the printing signal, None until there
    /// has been one.
    pub snr_db: Option<f32>,
    /// The mode of the signal printed last (the first tried when none has
    /// been).
    pub variant: String,
    /// Characters printed since the decoder was made.
    pub chars: u64,
}

// ---------------------------------------------------------------------------
// Varicode.

/// Every DominoEX code's value (nibbles, first one highest) to its byte,
/// with 0x100 added for the secondary alphabet; -1 where there is no code.
fn domino_decode_table() -> &'static [i16; 4096] {
    static TABLE: OnceLock<Box<[i16; 4096]>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = Box::new([-1i16; 4096]);
        for (i, code) in DOMINO_CODES.iter().enumerate() {
            t[domino_value(code)] = i as i16;
        }
        t
    })
}

fn domino_value(code: &[u8; 3]) -> usize {
    let mut v = code[0] as usize;
    for &n in &code[1..] {
        if n & 8 == 0 {
            break;
        }
        v = (v << 4) | n as usize;
    }
    v
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Decoded {
    Primary(u8),
    Secondary,
    Invalid,
}

/// What a Thor code (bits as sent, leading 1 included) is.
fn thor_decode(code: u32) -> Decoded {
    if code < 0xB80 {
        return varicode::decode(code).map_or(Decoded::Invalid, Decoded::Primary);
    }
    if THOR_SECONDARY.contains(&(code as u16)) {
        Decoded::Secondary
    } else {
        Decoded::Invalid
    }
}

/// What a DominoEX FEC code is: MFSK varicode, with the ranges MultiPSK's
/// scheme gives over to its secondary text set aside.
fn domino_fec_decode(code: u32) -> Decoded {
    match varicode::decode(code) {
        None => Decoded::Invalid,
        Some(1..=7 | 9..=12 | 14..=31 | 127..=159) => Decoded::Secondary,
        Some(c) => Decoded::Primary(c),
    }
}

/// The byte as text, if it is printable.
fn printable(c: u8) -> Option<char> {
    match c {
        b'\r' | b'\n' => Some('\n'),
        0x20..=0x7e => Some(c as char),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// A tracker.

fn step_valid(d: i32) -> bool {
    (-16..=-1).contains(&d) || (2..=17).contains(&d)
}

struct Track {
    /// Start of the next symbol's window, in samples since the stream began.
    pos: f64,
    /// Frequency of comb point 0 (`WING` tones below the first tone).
    f_lo: f64,
    level: [f32; WIN],
    prev_like: [f32; WIN],
    prev_peak: Option<usize>,
    noise: f32,
    noise_seen: bool,
    occupancy: [u32; WIN],
    sym: u64,
    history: u32,
    /// Each symbol's "good" flag until the tracker opens.
    good_log: Vec<bool>,
    // Plain DominoEX framing.
    nibbles: [u8; 3],
    nibble_count: usize,
    // The coded path (Thor and FEC DominoEX; for plain DominoEX, only the
    // check that the signal is not Thor).
    deinterleave: Interleaver<f32>,
    viterbi: Viterbi,
    encoder: Encoder,
    /// Received pairs waiting to be compared with the decoder's output.
    delay: Vec<(f32, f32)>,
    agree: f32,
    agree_seen: u32,
    shreg: u32,
    // Output.
    /// Read but not yet printed, with the symbol each came from.
    pending: Vec<(char, u64)>,
    make_sense: usize,
    no_sense: usize,
    open: bool,
    ended: Option<u64>,
    lines: Lines,
    last_char: u64,
    snr: f32,
    snr_seen: bool,
    quality: f32,
}

impl Track {
    /// The last symbol of the run of good ones, for where text stops; `fec`
    /// adds the decoding delay of a character on a coded path.
    fn last_good(&self, latency: u64, fec: bool) -> u64 {
        let mut last = RATE_WINDOW - 8;
        for i in 0..RATE_WINDOW.saturating_sub(8) {
            if ((self.history >> i) & 0xff).count_ones() >= 5 {
                last = i;
                break;
            }
        }
        let e = self.sym.saturating_sub(last as u64 + 1);
        if fec { e + latency } else { e.saturating_sub(TAIL_TRIM) }
    }

    /// Drop what was read before the run of good symbols began.
    fn trim_start(&mut self, latency: u64, fec: bool) {
        let log = &self.good_log;
        let begin = (0..log.len().saturating_sub(5))
            .find(|&i| log[i..i + 6].iter().filter(|g| **g).count() >= 5)
            .unwrap_or(0) as u64;
        let cut = if fec { begin + latency } else { begin + 4 };
        self.pending.retain(|&(_, s)| s >= cut);
        self.good_log = Vec::new();
    }
}

// ---------------------------------------------------------------------------
// One speed's receiver.

struct Chain {
    family: Family,
    label: String,
    /// Samples per symbol, comb step in search bins, rate.
    s: usize,
    ds: usize,
    rate: f32,
    baud: f64,
    spacing: f64,
    depth: usize,
    /// Symbols from a coded bit going in to its data bit coming out.
    latency: u64,

    ring: Vec<f32>,
    total: u64,
    since_hop: usize,
    hop_count: u64,
    fft: Arc<dyn Fft<f32>>,
    scratch: Vec<Complex32>,
    buf: Vec<Complex32>,

    // Search.
    lo_bin: usize,
    bins: usize,
    step: usize,
    row: Vec<f32>,
    level: Vec<f32>,
    whitened: Vec<f32>,
    sorted: Vec<f32>,
    score: Vec<f32>,
    prev_peak: Vec<u8>,
    /// Hop until which each bin is left alone.
    cool: Vec<u64>,

    tracks: Vec<Track>,
    last_center: Option<f32>,
    last_snr: Option<f32>,
    last_print: Option<u64>,
    chars: u64,
    fresh: Vec<IfkEvent>,
    low_bias: f32,
}

impl Chain {
    fn new(family: Family, speed: IfkSpeed) -> Self {
        let (s, ds, rate) = speed.timing();
        let label = match family {
            Family::Thor => format!("THOR{}", speed.name()),
            Family::DominoEx => format!("DOMEX{}", speed.name()),
            Family::DominoExFec => format!("DOMEX{}FEC", speed.name()),
        };
        // Plain DominoEX has no interleaver of its own: this one serves the
        // check that the signal is not Thor, so it is Thor's.
        let depth = match (family, speed) {
            (Family::DominoExFec, _) | (_, IfkSpeed::Micro) => 4,
            _ => 10,
        };
        let size = 2 * s;
        let fft = FftPlanner::new().plan_fft_forward(size);
        let scratch = vec![Complex32::new(0.0, 0.0); fft.get_inplace_scratch_len()];
        let bin_hz = rate / size as f32;
        let baud = (rate / s as f32) as f64;
        let spacing = baud * ds as f64;
        let half = 8.5 * spacing as f32;
        let lo_bin = (((SEARCH_LO - half) / bin_hz).floor() as usize).max(1);
        let bins = (((SEARCH_HI - SEARCH_LO) / bin_hz).ceil() as usize).max(1);
        let step = 2 * ds;
        let row_len = bins + 17 * step + 1;
        // Mean of the lowest quarter of WIN exponentials, against their mean.
        let m = (WIN / 4).max(1);
        let (mut bias, mut acc) = (0.0, 0.0);
        for i in 1..=m {
            acc += 1.0 / (WIN - i + 1) as f32;
            bias += acc;
        }
        let latency = (3 * depth + VITERBI_DELAY as usize / 2) as u64;
        Self {
            family,
            label,
            s,
            ds,
            rate,
            baud,
            spacing,
            depth,
            latency,
            ring: vec![0.0; RING_SYMBOLS * s],
            total: 0,
            since_hop: 0,
            hop_count: 0,
            fft,
            scratch,
            buf: vec![Complex32::new(0.0, 0.0); size],
            lo_bin,
            bins,
            step,
            row: vec![0.0; row_len],
            level: vec![0.0; row_len],
            whitened: vec![0.0; row_len],
            sorted: Vec::with_capacity(row_len),
            score: vec![0.0; PHASES * bins],
            prev_peak: vec![255; PHASES * bins],
            cool: vec![0; bins],
            tracks: Vec::new(),
            last_center: None,
            last_snr: None,
            last_print: None,
            chars: 0,
            fresh: Vec::new(),
            low_bias: bias / m as f32,
        }
    }

    fn at(&self, t: u64) -> f32 {
        self.ring[(t % self.ring.len() as u64) as usize]
    }

    fn oldest(&self) -> u64 {
        self.total.saturating_sub(self.ring.len() as u64)
    }

    fn push(&mut self, x: &[f32]) {
        for &v in x {
            let n = self.ring.len() as u64;
            self.ring[(self.total % n) as usize] = v;
            self.total += 1;
            self.since_hop += 1;
            if self.since_hop == self.s / PHASES {
                self.since_hop = 0;
                self.hop();
                self.advance_tracks();
            }
        }
    }

    /// The spectrum of the `s` samples from `start`, with `f` moved to bin 0.
    fn spectrum_at(&mut self, start: u64, f: f64) {
        let inc = -std::f64::consts::TAU * f / self.rate as f64;
        let (mut re, mut im) = (1.0f64, 0.0f64);
        let (cr, ci) = (inc.cos(), inc.sin());
        for i in 0..self.s {
            let v = self.at(start + i as u64) as f64;
            self.buf[i] = Complex32::new((v * re) as f32, (v * im) as f32);
            let nr = re * cr - im * ci;
            im = re * ci + im * cr;
            re = nr;
        }
        let size = self.buf.len();
        self.buf[self.s..size].fill(Complex32::new(0.0, 0.0));
        self.fft.process_with_scratch(&mut self.buf, &mut self.scratch);
    }

    /// Power at one frequency in the `s` samples from `start`.
    fn tone_power(&self, start: u64, f: f64) -> f32 {
        let inc = -std::f64::consts::TAU * f / self.rate as f64;
        let (mut re, mut im) = (1.0f64, 0.0f64);
        let (cr, ci) = (inc.cos(), inc.sin());
        let (mut sr, mut si) = (0.0f64, 0.0f64);
        for i in 0..self.s {
            let v = self.at(start + i as u64) as f64;
            sr += v * re;
            si += v * im;
            let nr = re * cr - im * ci;
            im = re * ci + im * cr;
            re = nr;
        }
        (sr * sr + si * si) as f32
    }

    // -----------------------------------------------------------------------
    // Search.

    fn hop(&mut self) {
        self.hop_count += 1;
        if (self.total as usize) < self.s {
            return;
        }
        let start = self.total - self.s as u64;
        let size = self.buf.len();
        for i in 0..self.s {
            let v = self.at(start + i as u64);
            self.buf[i] = Complex32::new(v, 0.0);
        }
        self.buf[self.s..size].fill(Complex32::new(0.0, 0.0));
        self.fft.process_with_scratch(&mut self.buf, &mut self.scratch);
        let n = self.row.len();
        for j in 0..n {
            self.row[j] = self.buf[self.lo_bin + j].norm_sqr();
        }
        self.sorted.clear();
        self.sorted.extend_from_slice(&self.row);
        let mid = n / 2;
        self.sorted.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
        let median = self.sorted[mid] / std::f32::consts::LN_2;
        let first = self.hop_count <= 4 + PHASES as u64;
        for j in 0..n {
            if first {
                self.level[j] = median;
            }
            let bg = self.level[j].max(0.25 * median).max(1e-20);
            self.whitened[j] = self.row[j] / bg;
            let clipped = self.row[j].min(LEVEL_CLIP * self.level[j].max(median * 0.5));
            self.level[j] += (clipped - self.level[j]) / SEARCH_LEVEL_TAU_HOPS;
        }
        if self.hop_count > 16 + PHASES as u64 {
            self.score_row();
        }
    }

    fn score_row(&mut self) {
        let phase = (self.hop_count % PHASES as u64) as usize;
        let base = phase * self.bins;
        let hop = self.hop_count;
        let (mut best, mut at) = (SCORE_START, None);
        for b in 0..self.bins {
            let (mut peak, mut second, mut k) = (0f32, 0f32, 0usize);
            for t in 0..TONES {
                let v = self.whitened[b + t * self.step];
                if v > peak {
                    second = peak;
                    peak = v;
                    k = t;
                } else if v > second {
                    second = v;
                }
            }
            let i = base + b;
            let prev = self.prev_peak[i];
            let valid = prev == 255 || step_valid(k as i32 - prev as i32);
            self.prev_peak[i] = k as u8;
            let s = if valid && peak >= SCORE_PURITY * second {
                peak.min(SCORE_PEAK_CAP) - SCORE_PEAK_BIAS
            } else {
                SCORE_BAD
            };
            let score = (self.score[i] + s - self.score[i] / SCORE_TAU).max(-SCORE_TAU);
            self.score[i] = score;
            if score > best && self.cool[b] <= hop {
                best = score;
                at = Some(b);
            }
        }
        if let Some(b) = at
            && self.tracks.len() < MAX_TRACKS
            && !self.covered(b)
        {
            self.start_track(b);
        }
    }

    /// The frequency of the lowest comb point for search bin `b`.
    fn search_freq(&self, b: usize) -> f64 {
        (self.lo_bin + b) as f64 * self.rate as f64 / self.buf.len() as f64
    }

    fn covered(&self, b: usize) -> bool {
        let f = self.search_freq(b);
        let half = TONES as f64 * self.spacing;
        self.tracks
            .iter()
            .any(|t| (t.f_lo + WING as f64 * self.spacing - f).abs() < half)
    }

    fn start_track(&mut self, b: usize) {
        let s = self.s as u64;
        let last = self.total - s;
        let avail = ((self.total - self.oldest()) / s).saturating_sub(2) as usize;
        let refine = REFINE_SYMBOLS.min(avail);
        if refine < 8 {
            return;
        }
        // The comb extended 11 tones down and 11 up: where the 18 tones
        // really are is found from what is seen.
        const SPAN: usize = 40;
        const BELOW: usize = 11;
        let spacing = self.spacing;
        let mut f_lo = self.search_freq(b) - BELOW as f64 * spacing;
        // Timing, then frequency, each against the other's best so far.
        let (mut best_tau, mut best_m) = (0i64, -1f32);
        // A symbol's worth of timing either way of the search's quarter
        // symbol: the search can fire a quarter symbol away from the best.
        for i in -8..=8i64 {
            let tau = i * (self.s as i64 / 16);
            let m = self.peak_metric(last, tau, f_lo, SPAN, refine);
            if m > best_m {
                best_m = m;
                best_tau = tau;
            }
        }
        let (mut best_df, mut best_m) = (0f64, -1f32);
        for i in -4..=4i32 {
            let df = i as f64 * self.baud / 8.0;
            let m = self.peak_metric(last, best_tau, f_lo + df, SPAN, refine);
            if m > best_m {
                best_m = m;
                best_df = df;
            }
        }
        f_lo += best_df;
        // And the timing again, finer, with the frequency known.
        let (base_tau, step) = (best_tau, (self.s as i64 / 64).max(1));
        let mut best_m = -1f32;
        for i in -5..=5i64 {
            let tau = base_tau + i * step;
            let m = self.peak_metric(last, tau, f_lo, SPAN, refine);
            if m > best_m {
                best_m = m;
                best_tau = tau;
            }
        }
        // Which 18 of the comb points the peaks use, and how far back the
        // signal goes.
        let mut hist = [0u32; SPAN];
        let mut good = Vec::with_capacity(RETRO_SYMBOLS);
        let scan = RETRO_SYMBOLS.min(avail);
        let mut pw = [0f32; SPAN];
        for j in 1..=scan {
            let t = last as i64 + best_tau - j as i64 * s as i64;
            if t < self.oldest() as i64 {
                good.push(false);
                continue;
            }
            self.spectrum_at(t as u64, f_lo);
            for (k, p) in pw.iter_mut().enumerate() {
                *p = self.buf[k * self.step].norm_sqr();
            }
            let (mut peak, mut at, mut total) = (0f32, 0usize, 0f32);
            for (k, &p) in pw.iter().enumerate() {
                total += p;
                if p > peak {
                    peak = p;
                    at = k;
                }
            }
            let rest = (total - peak) / (SPAN - 1) as f32;
            let g = peak > 6.0 * rest.max(1e-20);
            if g && j <= refine.max(32) {
                hist[at] += 1;
            }
            good.push(g);
        }
        // Many windows can tie when not every tone was sent: the middle of
        // the ties puts the tones furthest from the edges.
        let sums: Vec<u32> =
            (0..=SPAN - TONES).map(|s0| hist[s0..s0 + TONES].iter().sum()).collect();
        let best_sum = sums.iter().copied().max().unwrap_or(0);
        let ties: Vec<usize> = (0..sums.len()).filter(|&i| sums[i] == best_sum).collect();
        let shift = ties[ties.len() / 2];
        // Walk back to where runs of good symbols stop.
        let mut back = 0usize;
        for j in 0..good.len() {
            let span = &good[j..(j + 8).min(good.len())];
            if span.len() >= 4 && span.iter().filter(|g| **g).count() * 4 >= span.len() * 3 {
                back = j + span.iter().position(|g| *g).unwrap_or(0) + 1;
            } else if j > back + 8 {
                break;
            }
        }
        let back = back.max(refine / 2) as i64;
        let start = (last as i64 + best_tau - back * s as i64).max(self.oldest() as i64);
        let tone0 = f_lo + shift as f64 * spacing;
        let track = Track {
            pos: start as f64,
            f_lo: tone0 - WING as f64 * spacing,
            level: [0.0; WIN],
            prev_like: [0.0; WIN],
            prev_peak: None,
            noise: 0.0,
            noise_seen: false,
            occupancy: [0; WIN],
            sym: 0,
            history: 0,
            good_log: Vec::new(),
            nibbles: [0; 3],
            nibble_count: 0,
            deinterleave: Interleaver::new(4, self.depth, false, 0.0),
            viterbi: Viterbi::new(VITERBI_DELAY),
            encoder: Encoder::default(),
            delay: Vec::with_capacity(VITERBI_DELAY as usize + 2),
            agree: 0.0,
            agree_seen: 0,
            shreg: 1,
            pending: Vec::new(),
            make_sense: 0,
            no_sense: 0,
            open: false,
            ended: None,
            lines: Lines::default(),
            last_char: 0,
            snr: 0.0,
            snr_seen: false,
            quality: 0.0,
        };
        self.tracks.push(track);
    }

    /// The sum of each symbol's peak over the comb at one timing and
    /// frequency, for choosing among them.
    fn peak_metric(&mut self, last: u64, tau: i64, f_lo: f64, span: usize, n: usize) -> f32 {
        let mut sum = 0.0;
        for j in 1..=n {
            let t = last as i64 + tau - j as i64 * self.s as i64;
            if t < self.oldest() as i64 {
                continue;
            }
            self.spectrum_at(t as u64, f_lo);
            let mut peak = 0f32;
            for k in 0..span {
                peak = peak.max(self.buf[k * self.step].norm_sqr());
            }
            sum += peak;
        }
        sum
    }

    /// Leave a dropped signal's stretch of band alone for a while.
    fn cool_down(&mut self, f_lo: f64) {
        let bin_hz = self.rate as f64 / self.buf.len() as f64;
        let c = (f_lo + WING as f64 * self.spacing) / bin_hz - self.lo_bin as f64;
        let reach = (TONES * self.step) as f64;
        let lo = ((c - reach).max(0.0) as usize).min(self.bins);
        let hi = ((c + reach).max(0.0) as usize).min(self.bins);
        let until = self.hop_count + COOLDOWN_TAUS * (SCORE_TAU as u64) * PHASES as u64;
        for b in lo..hi {
            self.cool[b] = until;
            for p in 0..PHASES {
                self.score[p * self.bins + b] = -SCORE_TAU;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Tracking.

    fn advance_tracks(&mut self) {
        let mut i = 0;
        while i < self.tracks.len() {
            let needed = self.s as f64 * 1.25 + 2.0;
            let mut keep = true;
            while keep && self.tracks[i].pos + needed <= self.total as f64 {
                keep = self.step_track(i);
            }
            self.release(i, !keep);
            if keep {
                i += 1;
            } else {
                let t = self.tracks.swap_remove(i);
                self.finish_track(t);
            }
        }
    }

    fn finish_track(&mut self, mut t: Track) {
        if t.open
            && let Some(line) = t.lines.take()
        {
            self.emit(line, t.f_lo, t.snr);
        }
        self.cool_down(t.f_lo);
    }

    fn center_of(&self, f_lo: f64) -> f32 {
        (f_lo + (WING as f64 + 8.5) * self.spacing) as f32
    }

    /// One symbol; false when the tracker is finished.
    fn step_track(&mut self, idx: usize) -> bool {
        let start = self.tracks[idx].pos.round().max(self.oldest() as f64) as u64;
        let f_lo = self.tracks[idx].f_lo;
        self.spectrum_at(start, f_lo);
        let size = self.buf.len();
        let mut pw = [0f32; WIN];
        let mut e_lo = [0f32; WIN];
        let mut e_hi = [0f32; WIN];
        for k in 0..WIN {
            let c = k * self.step;
            pw[k] = self.buf[c].norm_sqr();
            e_lo[k] = self.buf[(c + size - 1) % size].norm_sqr();
            e_hi[k] = self.buf[c + 1].norm_sqr();
        }

        // Noise floor: the quietest tones of the comb.
        let mut sorted = pw;
        sorted.sort_unstable_by(|a, b| a.total_cmp(b));
        let m = (WIN / 4).max(1);
        let low = sorted[..m].iter().sum::<f32>() / m as f32 / self.low_bias;
        let t = &mut self.tracks[idx];
        if !t.noise_seen {
            t.noise = low;
            t.noise_seen = true;
            t.level = [low.max(1e-20); WIN];
        } else {
            t.noise += (low - t.noise) / 16.0;
        }
        let noise = t.noise.max(1e-20);
        let mut like = [0f32; WIN];
        for k in 0..WIN {
            let bg = t.level[k].max(0.7 * noise);
            like[k] = pw[k] / bg;
            let clipped = pw[k].min(LEVEL_CLIP * t.level[k].max(noise));
            t.level[k] += (clipped - t.level[k]) / TRACK_LEVEL_TAU_SYMBOLS;
        }
        let (mut peak, mut second, mut at) = (0f32, 0f32, 0usize);
        for (k, &l) in like.iter().enumerate() {
            if l > peak {
                second = peak;
                peak = l;
                at = k;
            } else if l > second {
                second = l;
            }
        }
        let valid = t.prev_peak.is_none_or(|p| step_valid(at as i32 - p as i32));
        // A real tone sits on the comb; another station's tone between comb
        // points shows as much or more half a baud to one side.
        let on_grid = e_lo[at].max(e_hi[at]) < GRID_MARGIN * pw[at];
        // Once a signal is known, a symbol far weaker than its symbols have
        // been is not one of them: this is what finds a signal's end cleanly.
        let strong = peak >= GOOD_PEAK.max(GOOD_QUALITY * t.quality);
        let good = valid && on_grid && strong && peak >= GOOD_PURITY * second;
        t.history = (t.history << 1) | good as u32;
        if !t.open {
            t.good_log.push(good);
        }

        // Likelihood of each nibble from the two tones at the step. A tone's
        // normalized power is its log likelihood up to `LIKELIHOOD_CAP`; above
        // that it grows only slowly, so a strong signal's sidelobes stay far
        // below its tone and a static crash cannot outvote the rest.
        let top = compress(peak);
        let mut lin = [0f32; WIN];
        for k in 0..WIN {
            lin[k] = (compress(like[k]) - top).exp();
        }
        let mut p = [0f32; 16];
        for k0 in 0..WIN {
            if t.prev_like[k0] <= 0.0 {
                continue;
            }
            for k1 in 0..WIN {
                let d = k1 as i32 - k0 as i32;
                if step_valid(d) {
                    let n = (d - 2).rem_euclid(TONES as i32) as usize;
                    p[n] += t.prev_like[k0] * lin[k1];
                }
            }
        }
        t.prev_like = lin;
        t.prev_peak = Some(at);

        let have_step = t.sym > 0;
        let mut bits = [0f32; 4];
        let mut nibble = 0usize;
        if have_step {
            let mut best = 0f32;
            for (n, &v) in p.iter().enumerate() {
                if v > best {
                    best = v;
                    nibble = n;
                }
            }
            for (bit, b) in bits.iter_mut().enumerate() {
                let mask = 8 >> bit;
                let (mut p0, mut p1) = (1e-30f32, 1e-30f32);
                for (n, &v) in p.iter().enumerate() {
                    if n & mask == 0 {
                        p0 += v;
                    } else {
                        p1 += v;
                    }
                }
                *b = (p0 / p1).ln().clamp(-LLR_MAX, LLR_MAX);
            }
        }

        // Timing and frequency follow the signal only on clear symbols.
        if good {
            t.occupancy[at] += 1;
            let snr_now =
                10.0 * ((peak - 1.0).max(0.01) * self.rate / (2500.0 * self.s as f32)).log10();
            if t.snr_seen {
                t.snr += (snr_now - t.snr) / 16.0;
            } else {
                t.snr = snr_now;
                t.snr_seen = true;
            }
            t.quality += (peak.min(40.0) - t.quality) / 32.0;
            let f = f_lo + (at * self.ds) as f64 * self.baud;
            // An eighth of a symbol either side: the windows hold little of
            // the neighbor symbols, whose tones would otherwise pull them.
            let eighth = (self.s / 8) as u64;
            let oldest = self.oldest();
            let e = self.tone_power(start.saturating_sub(eighth).max(oldest), f);
            let l = self.tone_power(start + eighth, f);
            let t = &mut self.tracks[idx];
            let sum = e + l;
            if sum > 0.0 {
                // Late by x shows an error of about -2x / (7/8 of a symbol).
                let err = ((l - e) / sum).clamp(-1.0, 1.0) as f64;
                let gain = if t.sym < 16 { 0.2 } else { 0.08 };
                t.pos += gain * err * 0.44 * self.s as f64;
            }
            let (el, eh) = (e_lo[at], e_hi[at]);
            if el + eh > 0.0 {
                let err = ((eh - el) / (el + eh)) as f64;
                t.f_lo += 0.04 * err * self.baud;
            }
        }
        let t = &mut self.tracks[idx];
        t.pos += self.s as f64;
        let sym = t.sym;
        t.sym += 1;

        // Keep the 18 tones centered in the comb. A window must beat the
        // present one by a clear margin: in 32 symbols not every tone gets
        // used, and many windows tie.
        if sym % 32 == 31 {
            let sum_at = |s0: usize| -> u32 { t.occupancy[s0..s0 + TONES].iter().sum() };
            let here = sum_at(WING);
            let (mut best, mut shift) = (here + 2, WING);
            for s0 in 0..=WIN - TONES {
                if sum_at(s0) > best {
                    best = sum_at(s0);
                    shift = s0;
                }
            }
            if shift != WING {
                let d = shift as i32 - WING as i32;
                t.f_lo += d as f64 * self.spacing;
                let (old_level, old_like) = (t.level, t.prev_like);
                for k in 0..WIN {
                    let j = k as i32 + d;
                    let inside = (0..WIN as i32).contains(&j);
                    t.level[k] = if inside { old_level[j as usize] } else { noise };
                    t.prev_like[k] = if inside { old_like[j as usize] } else { 0.0 };
                }
                t.prev_peak = t.prev_peak.map(|p| (p as i32 - d).clamp(0, WIN as i32 - 1) as usize);
            }
            t.occupancy = [0; WIN];
        }

        if have_step {
            self.feed(idx, nibble as u8, bits, sym);
        }
        self.judge(idx)
    }

    /// One nibble's worth of decoding.
    fn feed(&mut self, idx: usize, nibble: u8, bits: [f32; 4], sym: u64) {
        let family = self.family;
        let t = &mut self.tracks[idx];
        if family == Family::DominoEx {
            domino_nibble(t, nibble, sym);
        }
        let mut b = bits;
        t.deinterleave.symbols(&mut b);
        for pair in b.chunks(2) {
            let (l0, l1) = (pair[0], pair[1]);
            t.delay.push((l0, l1));
            let Some(bit) = t.viterbi.push(l0, l1) else {
                continue;
            };
            let (a, c) = t.encoder.push(bit);
            let (d0, d1) = t.delay.remove(0);
            let matched = soft_match(a, d0) + soft_match(c, d1);
            let weight = d0.abs() + d1.abs();
            if weight > 0.0 {
                // A plain average at first, so the start of a run counts.
                t.agree_seen = t.agree_seen.saturating_add(1);
                t.agree += (matched / weight - t.agree) / (t.agree_seen as f32).min(AGREE_TAU);
            }
            if family != Family::DominoEx {
                t.shreg = (t.shreg << 1) | bit as u32;
                if t.shreg & 7 == 1 {
                    let code = t.shreg >> 1;
                    t.shreg = 1;
                    let d = if family == Family::Thor {
                        thor_decode(code)
                    } else {
                        domino_fec_decode(code)
                    };
                    accept(t, d, sym);
                } else if t.shreg >= 1 << 14 {
                    t.shreg = 0;
                }
            }
        }
    }

    /// Open, carry on or finish a tracker.
    fn judge(&mut self, idx: usize) -> bool {
        let family = self.family;
        let latency = self.latency;
        let t = &mut self.tracks[idx];
        let rate = (t.history & ((1 << RATE_WINDOW) - 1)).count_ones();
        if let Some(e) = t.ended {
            return t.sym <= e + HOLD_SYMBOLS;
        }
        let fec = family != Family::DominoEx;
        if !t.open {
            if t.sym >= RATE_WINDOW as u64 && rate < RATE_DROP {
                return false;
            }
            if t.sym >= OPEN_TIMEOUT {
                return false;
            }
            let agree_ok = t.agree_seen >= AGREE_MIN_PAIRS
                && if fec {
                    t.agree >= AGREE_OPEN
                } else {
                    t.agree < AGREE_IS_THOR
                };
            if rate >= RATE_OPEN
                && t.make_sense >= OPEN_CHARS
                && t.no_sense * 4 <= t.make_sense
                && agree_ok
            {
                t.open = true;
                t.trim_start(latency, fec);
            }
            return true;
        }
        if rate < RATE_END {
            let e = t.last_good(latency, fec);
            t.ended = Some(e);
            t.pending.retain(|&(_, s)| s <= e);
        }
        true
    }

    /// Print what has waited long enough.
    fn release(&mut self, idx: usize, all: bool) {
        if !self.tracks[idx].open {
            return;
        }
        let ready: Vec<char> = {
            let t = &mut self.tracks[idx];
            let cut = if all || t.ended.is_some() {
                u64::MAX
            } else {
                t.sym.saturating_sub(HOLD_SYMBOLS)
            };
            let n = t.pending.iter().take_while(|e| e.1 <= cut).count();
            t.pending.drain(..n).map(|e| e.0).collect()
        };
        if ready.is_empty() {
            return;
        }
        let f_lo = self.tracks[idx].f_lo;
        let snr = self.tracks[idx].snr;
        for ch in ready {
            if ch != '\n' {
                self.chars += 1;
            }
            self.tracks[idx].last_char = self.total;
            if let Some(line) = self.tracks[idx].lines.push(ch) {
                self.emit(line, f_lo, snr);
            }
        }
        self.last_print = Some(self.total);
        self.last_center = Some(self.center_of(f_lo));
        self.last_snr = Some(snr);
    }

    fn emit(&mut self, text: String, f_lo: f64, snr: f32) {
        if text.chars().filter(|c| !c.is_whitespace()).count() < 2 {
            return;
        }
        self.fresh.push(IfkEvent::Text {
            text,
            mode: self.label.clone(),
            center_hz: self.center_of(f_lo),
            snr_db: snr,
        });
    }

    fn flush(&mut self) {
        let (latency, fec) = (self.latency, self.family != Family::DominoEx);
        for i in 0..self.tracks.len() {
            let t = &mut self.tracks[i];
            if t.open && t.ended.is_none() {
                let e = t.last_good(latency, fec);
                t.pending.retain(|&(_, s)| s <= e);
            }
            self.release(i, true);
            let t = &mut self.tracks[i];
            if t.open {
                let (f_lo, snr) = (t.f_lo, t.snr);
                if let Some(line) = t.lines.take() {
                    self.emit(line, f_lo, snr);
                }
            }
        }
    }

    fn idle_flush(&mut self) {
        let total = self.total;
        for i in 0..self.tracks.len() {
            let t = &mut self.tracks[i];
            if t.open
                && !t.lines.is_empty()
                && total.saturating_sub(t.last_char) as f32 > FLUSH_S * self.rate
            {
                let (f_lo, snr) = (t.f_lo, t.snr);
                if let Some(line) = t.lines.take() {
                    self.emit(line, f_lo, snr);
                }
            }
        }
    }

    fn locked(&self) -> bool {
        self.tracks.iter().any(|t| t.open && t.ended.is_none())
    }

    fn open_band(&self) -> Option<(f32, f32)> {
        let half = (TONES as f64 * self.spacing) as f32 / 2.0;
        self.tracks
            .iter()
            .find(|t| t.open && t.ended.is_none())
            .map(|t| {
                let c = self.center_of(t.f_lo);
                (c - half, c + half)
            })
    }
}

/// The log likelihood a normalized power counts for.
fn compress(x: f32) -> f32 {
    if x <= LIKELIHOOD_CAP {
        x
    } else {
        LIKELIHOOD_CAP + 8.0 * (x / LIKELIHOOD_CAP).ln()
    }
}

/// A coded bit against what was received: the ratio's size if they agree.
fn soft_match(bit: bool, l: f32) -> f32 {
    // A positive ratio says a 0 bit.
    if bit != (l > 0.0) { l.abs() } else { 0.0 }
}

fn domino_nibble(t: &mut Track, c: u8, sym: u64) {
    if c & 8 == 0 {
        if t.nibble_count > 0 {
            let d = if t.nibble_count <= 3 {
                let mut value = 0usize;
                for &n in &t.nibbles[..t.nibble_count] {
                    value = (value << 4) | n as usize;
                }
                match domino_decode_table()[value] {
                    -1 => Decoded::Invalid,
                    v if v >= 0x100 => Decoded::Secondary,
                    v => Decoded::Primary(v as u8),
                }
            } else {
                Decoded::Invalid
            };
            accept(t, d, sym);
        }
        t.nibble_count = 0;
    }
    if t.nibble_count < 3 {
        t.nibbles[t.nibble_count] = c;
    }
    t.nibble_count = (t.nibble_count + 1).min(4);
}

/// A decoded code into the text so far.
fn accept(t: &mut Track, d: Decoded, sym: u64) {
    match d {
        Decoded::Primary(c) => {
            if let Some(ch) = printable(c) {
                if ch != '\n' {
                    t.make_sense += 1;
                }
                t.pending.push((ch, sym));
            } else if c != 0 && c != 2 && c != 4 {
                t.no_sense += 1;
            }
        }
        Decoded::Secondary => {}
        Decoded::Invalid => t.no_sense += 1,
    }
}

// ---------------------------------------------------------------------------
// The shared receiver.

struct Stream {
    rate: f32,
    resampler: Resampler,
    blanker: crate::rtty::Blanker,
    chains: Vec<usize>,
}

struct Receiver {
    streams: Vec<Stream>,
    chains: Vec<Chain>,
    center_hz: f32,
    tmp: Vec<f32>,
    blanked: Vec<f32>,
    events: Vec<IfkEvent>,
}

impl Receiver {
    fn new(fs: f32, family: Family, speeds: &[IfkSpeed], center_hz: f32) -> Self {
        let chains: Vec<Chain> = speeds.iter().map(|&s| Chain::new(family, s)).collect();
        let mut streams: Vec<Stream> = Vec::new();
        for (i, c) in chains.iter().enumerate() {
            if let Some(st) = streams.iter_mut().find(|st| st.rate == c.rate) {
                st.chains.push(i);
            } else {
                streams.push(Stream {
                    rate: c.rate,
                    resampler: Resampler::new(fs, c.rate, 3000.0, 5000.0),
                    blanker: crate::rtty::Blanker::new(c.rate),
                    chains: vec![i],
                });
            }
        }
        Self {
            streams,
            chains,
            center_hz,
            tmp: Vec::with_capacity(4096),
            blanked: Vec::with_capacity(4096),
            events: Vec::new(),
        }
    }

    fn push(&mut self, audio: &[f32]) {
        for st in self.streams.iter_mut() {
            self.tmp.clear();
            st.resampler.process(audio, &mut self.tmp);
            self.blanked.clear();
            for &x in &self.tmp {
                if let Some((y, _)) = st.blanker.push(x) {
                    self.blanked.push(y);
                }
            }
            for &i in &st.chains {
                self.chains[i].push(&self.blanked);
            }
            let band = st.chains.iter().find_map(|&i| self.chains[i].open_band());
            st.blanker.set_pair(band);
        }
        self.arbitrate();
        for c in self.chains.iter_mut() {
            c.idle_flush();
            self.events.append(&mut c.fresh);
        }
    }

    /// Two speeds reading the same stretch of band cannot both be right:
    /// the one reading it worse stops. One that has not opened gives way to
    /// one that has.
    fn arbitrate(&mut self) {
        let mut open: Vec<(usize, f32, f32, f32)> = Vec::new();
        for (ci, c) in self.chains.iter().enumerate() {
            for t in c.tracks.iter().filter(|t| t.open && t.ended.is_none()) {
                open.push((ci, c.center_of(t.f_lo), c.spacing as f32 * TONES as f32 * 0.5, t.quality));
            }
        }
        for ci in 0..self.chains.len() {
            let half = self.chains[ci].spacing as f32 * TONES as f32 * 0.5;
            let mut i = 0;
            while i < self.chains[ci].tracks.len() {
                let t = &self.chains[ci].tracks[i];
                let center = self.chains[ci].center_of(t.f_lo);
                let (is_open, quality, ended) = (t.open, t.quality, t.ended.is_some());
                let beaten = open.iter().any(|o| {
                    o.0 != ci && (o.1 - center).abs() < o.2 + half && (!is_open || o.3 > quality)
                });
                if beaten && !ended {
                    let t = self.chains[ci].tracks.swap_remove(i);
                    self.chains[ci].cool_down(t.f_lo);
                } else {
                    i += 1;
                }
            }
        }
    }

    fn take(&mut self) -> Vec<IfkEvent> {
        std::mem::take(&mut self.events)
    }

    fn flush(&mut self) {
        for c in self.chains.iter_mut() {
            c.flush();
            self.events.append(&mut c.fresh);
        }
    }

    fn status(&self) -> IfkStatus {
        let c = self
            .chains
            .iter()
            .filter(|c| c.last_print.is_some())
            .max_by_key(|c| c.last_print)
            .unwrap_or(&self.chains[0]);
        IfkStatus {
            locked: self.chains.iter().any(|c| c.locked()),
            center_hz: c.last_center.unwrap_or(self.center_hz),
            snr_db: c.last_snr,
            variant: c.label.clone(),
            chars: self.chains.iter().map(|c| c.chars).sum(),
        }
    }
}

// ---------------------------------------------------------------------------
// The decoders.

/// A DominoEX receiver searching 500 to 2500 Hz of the audio.
pub struct DominoExDecoder(Receiver);

impl DominoExDecoder {
    /// One speed, sent without FEC (fldigi's default).
    pub fn new(fs: f32, speed: IfkSpeed, center_hz: f32) -> Self {
        Self(Receiver::new(fs, Family::DominoEx, &[speed], center_hz))
    }

    /// One speed with MultiPSK-compatible FEC.
    pub fn new_fec(fs: f32, speed: IfkSpeed, center_hz: f32) -> Self {
        Self(Receiver::new(fs, Family::DominoExFec, &[speed], center_hz))
    }

    /// All the speeds at once, without FEC.
    pub fn new_auto(fs: f32, center_hz: f32) -> Self {
        Self(Receiver::new(fs, Family::DominoEx, &IfkSpeed::ALL, center_hz))
    }

    pub fn push(&mut self, audio: &[f32]) {
        self.0.push(audio);
    }

    pub fn take(&mut self) -> Vec<IfkEvent> {
        self.0.take()
    }

    pub fn flush(&mut self) {
        self.0.flush();
    }

    pub fn status(&self) -> IfkStatus {
        self.0.status()
    }
}

/// A Thor receiver searching 500 to 2500 Hz of the audio.
pub struct ThorDecoder(Receiver);

impl ThorDecoder {
    pub fn new(fs: f32, speed: IfkSpeed, center_hz: f32) -> Self {
        Self(Receiver::new(fs, Family::Thor, &[speed], center_hz))
    }

    /// All the speeds at once.
    pub fn new_auto(fs: f32, center_hz: f32) -> Self {
        Self(Receiver::new(fs, Family::Thor, &IfkSpeed::ALL, center_hz))
    }

    pub fn push(&mut self, audio: &[f32]) {
        self.0.push(audio);
    }

    pub fn take(&mut self) -> Vec<IfkEvent> {
        self.0.take()
    }

    pub fn flush(&mut self) {
        self.0.flush();
    }

    pub fn status(&self) -> IfkStatus {
        self.0.status()
    }
}
