//! MIL-STD-188-110A serial tone (and the single-tone modes NATO shares
//! with it): 2400 Bd 8-PSK at 1800 Hz.
//!
//! **Preamble.** Three 200 ms segments (24 for the long interleaver), each
//! fifteen channel symbols of 32 8-PSK symbols: 0, 1, 3, 0, 1, 3, 1, 2, 0,
//! D1, D2, C1, C2, C3, 0. A channel symbol is one of eight BPSK patterns
//! (four repeats of an 8-chip Walsh-like word) added to a fixed 32-symbol
//! scrambling sequence. D1 and D2 name the data rate and interleaver;
//! C1-C3 count the segments left. The receiver correlates the nine fixed
//! channel symbols, each in halves so a carrier 50 Hz off costs little,
//! reads D1-C3 as the best-matching patterns, and so knows the mode and
//! where the data begins before it arrives.
//!
//! **Data.** Frames of 32 data symbols and 16 known (20 and 20 below
//! 2400 bps), all scrambled by a 12-stage generator (x^12 + x^6 + x^4 +
//! x + 1 from 0xBAD, eight shifts a symbol, restarting every 160). The
//! known symbols are zeros except the two before each interleaver block,
//! which repeat D1 and D2. Bits are rate 1/2 coded (repeated twice at 300
//! and four times at 150 bps), block interleaved (40 rows, loaded down the
//! rows nine apart, fetched on diagonals seventeen columns back), and
//! mapped to the phases by a modified Gray code. The equalizer trains on
//! the whole preamble, then on every probe; decisions on the data keep it
//! moving between them.
//!
//! The short interleaver setting may be 0.0 or 0.6 s without the preamble
//! saying which; both are decoded and the one whose code checks wins.
//! 75 bps sends no probes: each pair of coded bits is one of four 32-symbol
//! Walsh words (with its second half inverted at interleaver block ends).
//! Those are BPSK once descrambled, so the equalizer runs on symbol
//! decisions as at 600 bps, while the bits come from correlating each
//! whole word with the four. Secure voice is identified from the preamble
//! but not decoded: it is digitized speech, encrypted.

use super::fec::{BlockInterleaver, Viterbi};
use super::modem::{C32, Clock, Constellation, Dfe, NF, PRE, Stream, confident_error, lambda_for, psk, rad_to_hz};
use super::s5066::{Dpdu, DpduSearch};
use std::collections::VecDeque;

const BAUD: f32 = 2400.0;
/// The preamble's scrambling sequence, one per symbol of a channel symbol.
const PRE_SCRAMBLE: [u8; 32] = [7, 4, 3, 0, 5, 1, 5, 0, 2, 2, 1, 1, 5, 7, 4, 3, 5, 0, 2, 6, 2, 1, 6, 2, 0, 0, 5, 0, 5, 2, 6, 6];
/// The eight channel-symbol words, each sent four times.
const WORDS: [[u8; 8]; 8] = [
    [0, 0, 0, 0, 0, 0, 0, 0],
    [0, 4, 0, 4, 0, 4, 0, 4],
    [0, 0, 4, 4, 0, 0, 4, 4],
    [0, 4, 4, 0, 0, 4, 4, 0],
    [0, 0, 0, 0, 4, 4, 4, 4],
    [0, 4, 0, 4, 4, 0, 4, 0],
    [0, 0, 4, 4, 4, 4, 0, 0],
    [0, 4, 4, 0, 4, 0, 0, 4],
];
/// The fixed channel symbols of a segment, slots 0 to 8.
const FIXED: [usize; 9] = [0, 1, 3, 0, 1, 3, 1, 2, 0];
const SLOT: usize = 32;
const SEGMENT: usize = 15 * SLOT;
/// Samples in a slot and a segment, at two a symbol.
const SLOT_N: u64 = 2 * SLOT as u64;
const SEGMENT_N: u64 = 2 * SEGMENT as u64;

const fn data_scrambler() -> [u8; 160] {
    let mut s = [0u8; 160];
    let mut reg: u16 = 0xbad;
    let mut i = 0;
    while i < 160 {
        let mut k = 0;
        while k < 8 {
            let carry = (reg >> 11) & 1;
            reg = (reg << 1) & 0xfff;
            if carry == 1 {
                reg ^= 0x053;
            }
            k += 1;
        }
        s[i] = (reg & 7) as u8;
        i += 1;
    }
    s
}

const SCRAMBLE: [u8; 160] = data_scrambler();

/// Tribit phase for each modified-Gray label: 3 bits (first sent highest)
/// at 2400 and 4800 bps, 2 at 1200.
const MGD3: [usize; 8] = [0, 1, 3, 2, 7, 6, 4, 5];
const MGD2: [usize; 4] = [0, 2, 6, 4];
/// At 75 bps the modified-Gray value of each label picks the Walsh word.
const MGD75: [usize; 4] = [0, 1, 3, 2];
/// The end-of-message pattern, first bit sent highest.
const EOM: u32 = 0x4B65_A5B2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Interleave {
    Zero,
    Short,
    Long,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Mode {
    pub(super) bps: u32,
    pub(super) interleave: Interleave,
    pub(super) voice: bool,
    d1: u8,
    d2: u8,
}

impl Mode {
    /// The rate and interleaver from D1 and D2 (table XV), with the short
    /// setting standing for both 0.0 and 0.6 s.
    pub(super) fn from_d(d1: u8, d2: u8) -> Option<Self> {
        let (bps, il, voice) = match (d1, d2) {
            (7, 6) => (4800, Interleave::Short, false),
            (7, 7) => (2400, Interleave::Short, true),
            (6, 4) => (2400, Interleave::Short, false),
            (4, 4) => (2400, Interleave::Long, false),
            (6, 5) => (1200, Interleave::Short, false),
            (4, 5) => (1200, Interleave::Long, false),
            (6, 6) => (600, Interleave::Short, false),
            (4, 6) => (600, Interleave::Long, false),
            (6, 7) => (300, Interleave::Short, false),
            (4, 7) => (300, Interleave::Long, false),
            (7, 4) => (150, Interleave::Short, false),
            (5, 4) => (150, Interleave::Long, false),
            (7, 5) => (75, Interleave::Short, false),
            (5, 5) => (75, Interleave::Long, false),
            _ => return None,
        };
        Some(Self { bps, interleave: il, voice, d1, d2 })
    }

    pub(super) fn coded(&self) -> bool {
        self.bps < 4800
    }

    pub(super) fn modulation(&self) -> &'static str {
        match self.bps {
            4800 | 2400 => "8-PSK",
            1200 => "QPSK",
            75 => "Walsh-coded QPSK",
            _ => "BPSK",
        }
    }

    pub(super) fn interleave_label(&self) -> &'static str {
        match (self.bps, self.interleave) {
            (4800, _) => "none",
            (_, Interleave::Zero) => "zero (0.0 s)",
            (_, Interleave::Short) => "short (0.6 s)",
            (_, Interleave::Long) => "long (4.8 s)",
        }
    }

    /// Unknown and known symbols a frame.
    fn frame(&self) -> (usize, usize) {
        match self.bps {
            4800 | 2400 => (32, 16),
            75 => (32, 0),
            _ => (20, 20),
        }
    }

    fn rep(&self) -> usize {
        match self.bps {
            300 => 2,
            150 => 4,
            _ => 1,
        }
    }

    fn constellation(&self) -> Constellation {
        match self.bps {
            4800 | 2400 => Constellation::from_phases(&MGD3, 3),
            1200 => Constellation::from_phases(&MGD2, 2),
            _ => Constellation::from_phases(&[0, 4], 1),
        }
    }

    /// Rows and columns of the interleaver at this setting.
    fn matrix(&self, il: Interleave) -> Option<(usize, usize)> {
        let long = il == Interleave::Long;
        match (self.bps, il) {
            (4800, _) | (_, Interleave::Zero) => None,
            (2400, _) => Some((40, if long { 576 } else { 72 })),
            (1200, _) => Some((40, if long { 288 } else { 36 })),
            (75, _) => Some(if long { (20, 36) } else { (10, 9) }),
            _ => Some((40, if long { 144 } else { 18 })),
        }
    }

    /// Symbols an interleaver block spans, probes included; the D1/D2
    /// probes mark its end even when the interleaver is bypassed.
    fn block_symbols(&self) -> usize {
        if self.interleave == Interleave::Long { 11520 } else { 1440 }
    }

    fn decodable(&self) -> bool {
        !self.voice
    }

    /// The interleaver's load and fetch steps.
    fn steps(&self) -> (usize, usize) {
        if self.bps == 75 { (7, 7) } else { (9, 17) }
    }

    /// A 75 bps Walsh word is exceptional (second half inverted) when it is
    /// the last of its interleaver block: every 45th short, 360th long.
    fn exceptional(&self, set: usize) -> bool {
        let every = if self.interleave == Interleave::Long { 360 } else { 45 };
        (set + 1).is_multiple_of(every)
    }
}

/// The normalized correlation of a segment's fixed slots that counts as a
/// preamble: 1 for a clean one, 0.0625 for noise with a spread of 0.015.
/// D1 to C3 must then read as a valid mode and count besides.
const DETECT: f32 = 0.18;
/// The fixed slots shifted three slots still match three of nine (0, 1,
/// 3 starts the sequence twice), a metric near 0.33; a peak is final only
/// once nothing stronger turns up past that.
const PEAK_GUARD: u64 = 3 * SLOT_N + 16;
const RING: usize = 1024;

#[derive(Clone, Copy)]
struct Found {
    n: u64,
    metric: f32,
}

struct Detector {
    next: u64,
    /// Per sample: power of the correlation with each fixed word, its
    /// turn between halves, and the energy, a ring indexed by sample.
    m: Vec<[f32; 4]>,
    turn: Vec<[C32; 4]>,
    e: Vec<f32>,
    best: Option<Found>,
    score: f32,
}

impl Detector {
    fn new() -> Self {
        Self { next: 0, m: vec![[0.0; 4]; RING], turn: vec![[C32::default(); 4]; RING], e: vec![0.0; RING], best: None, score: 0.0 }
    }

    /// Run over what has arrived; a segment start once one stands out.
    fn run(&mut self, s: &Stream) -> Option<u64> {
        self.next = self.next.max(s.start());
        let mut out = None;
        while self.next + 2 * SLOT as u64 <= s.end() {
            let n = self.next;
            self.next += 1;
            let x = s.slice(n, 2 * SLOT).unwrap();
            // Descramble once; the words are sign patterns over it.
            let mut y = [C32::default(); SLOT];
            let mut e = 0.0;
            for k in 0..SLOT {
                y[k] = x[2 * k] * psk(8, PRE_SCRAMBLE[k] as usize).conj();
                e += y[k].norm_sqr();
            }
            let r = (n % RING as u64) as usize;
            self.e[r] = e;
            for (w, word) in WORDS.iter().take(4).enumerate() {
                let mut a = C32::default();
                let mut b = C32::default();
                for (k, &yk) in y.iter().enumerate() {
                    let v = if word[k % 8] == 0 { yk } else { -yk };
                    if k < SLOT / 2 {
                        a += v;
                    } else {
                        b += v;
                    }
                }
                self.m[r][w] = a.norm_sqr() + b.norm_sqr();
                self.turn[r][w] = b * a.conj();
            }
            // The segment that would start eight slots back is now whole
            // as far as its fixed part goes.
            let Some(n0) = n.checked_sub(8 * SLOT_N) else { continue };
            if n0 < s.start() {
                continue;
            }
            let mut p = 0.0;
            let mut en = 0.0;
            for (i, &w) in FIXED.iter().enumerate() {
                let ri = ((n0 + i as u64 * SLOT_N) % RING as u64) as usize;
                p += self.m[ri][w];
                en += self.e[ri];
            }
            let metric = p / ((SLOT / 2) as f32 * en.max(1e-20));
            self.score = (self.score * 0.9995).max(metric);
            if metric > DETECT && self.best.is_none_or(|b| metric > b.metric) {
                self.best = Some(Found { n: n0, metric });
            }
            if let Some(b) = self.best
                && n0 > b.n + PEAK_GUARD
            {
                self.best = None;
                out = Some(b.n);
                break;
            }
        }
        out
    }

    /// The carrier's turn per symbol over a segment's fixed slots.
    fn turn(&self, n0: u64) -> f32 {
        let mut t = C32::default();
        for (i, &w) in FIXED.iter().enumerate() {
            let ri = ((n0 + i as u64 * SLOT_N) % RING as u64) as usize;
            t += self.turn[ri][w];
        }
        t.arg() / (SLOT / 2) as f32
    }
}

/// One way of decoding the data bits.
struct Decoder {
    mode: Mode,
    il: Option<BlockInterleaver>,
    block: Vec<f32>,
    tmp: Vec<f32>,
    pend: Vec<f32>,
    vit: Viterbi,
    bits: Vec<u8>,
    search: DpduSearch,
    reg: u32,
    eom: bool,
}

impl Decoder {
    fn new(mode: Mode, il: Interleave) -> Self {
        let (load, fetch) = mode.steps();
        let il = mode.matrix(il).map(|(r, c)| BlockInterleaver::new(r, c, load, fetch));
        Self {
            mode,
            il,
            block: Vec::new(),
            tmp: Vec::new(),
            pend: Vec::new(),
            vit: Viterbi::new(),
            bits: Vec::new(),
            search: DpduSearch::new(),
            reg: 0,
            eom: false,
        }
    }

    fn push(&mut self, soft: &[f32]) {
        self.bits.clear();
        if !self.mode.coded() {
            self.bits.extend(soft.iter().map(|&v| u8::from(v < 0.0)));
        } else {
            match &self.il {
                Some(il) => {
                    self.block.extend_from_slice(soft);
                    if self.block.len() >= il.len() {
                        self.tmp.clear();
                        il.deinterleave(&self.block[..il.len()], &mut self.tmp);
                        self.block.drain(..il.len());
                        self.pend.extend_from_slice(&self.tmp);
                    }
                }
                None => self.pend.extend_from_slice(soft),
            }
            let rep = self.mode.rep();
            let mut used = 0;
            while self.pend.len() - used >= 2 * rep {
                let g = &self.pend[used..used + 2 * rep];
                let a: f32 = g.iter().step_by(2).sum();
                let b: f32 = g.iter().skip(1).step_by(2).sum();
                self.vit.push(a, b);
                used += 2 * rep;
            }
            self.pend.drain(..used);
            self.vit.take(&mut self.bits);
        }
        for &b in &self.bits {
            self.reg = self.reg << 1 | u32::from(b);
            if self.reg == EOM {
                self.eom = true;
            }
        }
        self.search.push_bits(&self.bits);
    }

    /// Whether the code checks, for choosing between interleaver guesses.
    fn ber(&self) -> f32 {
        if self.mode.coded() { self.vit.ber().unwrap_or(0.5) } else { 0.0 }
    }
}

struct Run {
    mode: Mode,
    /// Segments whose count agreed on where the data begins.
    votes: u32,
    /// Sample of the reference path for preamble symbol 0 of the segment
    /// first seen; moved by the clock tracking.
    n0: u64,
    count: u32,
    /// Symbols from that segment's start to the first data symbol.
    data_at: usize,
    /// The next symbol to process, counted from `n0`.
    j: usize,
    dfe: Dfe,
    var: f32,
    bad: u32,
    known_err: f32,
    known_n: u32,
    cons: Constellation,
    decoders: Vec<Decoder>,
    soft: Vec<f32>,
    zs: Vec<C32>,
    /// A 75 bps word's equalizer outputs, as sent and descrambled, and the
    /// equalizer as it was at the word's start.
    walsh: Vec<(C32, C32)>,
    word_start: Option<Dfe>,
    last_probe: f32,
    confident: f32,
    clock: Clock,
    since_clock: usize,
    done: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Note {
    Mode(Mode),
    Dpdu(Mode, Dpdu),
    Eom,
}

pub(super) struct Rx110 {
    det: Detector,
    run: Option<Box<Run>>,
    notes: Vec<Note>,
    shown: VecDeque<C32>,
    /// A segment found but not yet whole enough to read D1 to C3.
    pending: Option<u64>,
    /// A segment seen during another transmission's data, waiting for the
    /// next to confirm it: its start, mode and count.
    candidate: Option<(u64, Mode, u32)>,
}

/// Symbols of bad probes in a row (three seconds) before the data is given
/// up. There is no way back in without a preamble, so a deep fade is
/// waited out; a new transmission's preamble takes over at once anyway.
const LOST_SYMBOLS: usize = 7200;
const BAD_MSE: f32 = 0.9;

impl Rx110 {
    pub(super) fn new() -> Self {
        Self { det: Detector::new(), run: None, notes: Vec::new(), shown: VecDeque::new(), pending: None, candidate: None }
    }

    pub(super) fn active(&self) -> bool {
        self.run.is_some()
    }

    pub(super) fn mode(&self) -> Option<Mode> {
        self.run.as_ref().map(|r| r.mode)
    }

    pub(super) fn score(&self) -> f32 {
        self.det.score
    }

    pub(super) fn freq_error_hz(&self) -> Option<f32> {
        self.run.as_ref().map(|r| rad_to_hz(r.dfe.freq(), BAUD))
    }

    pub(super) fn snr_db(&self) -> Option<f32> {
        self.run.as_ref().filter(|r| r.j > 2 * SLOT).map(|r| 10.0 * (0.8 / r.var.max(1e-4)).log10())
    }

    pub(super) fn shown(&self) -> Vec<C32> {
        self.shown.iter().copied().collect()
    }

    pub(super) fn take_notes(&mut self) -> Vec<Note> {
        std::mem::take(&mut self.notes)
    }

    pub(super) fn needs_from(&self) -> u64 {
        let mut n = self.det.next.saturating_sub(16 * SLOT_N + 64);
        if let Some(p) = self.pending {
            n = n.min(p);
        }
        if let Some(r) = &self.run {
            n = n.min((r.n0 + 2 * r.j as u64).saturating_sub(PRE as u64 + 4));
        }
        n
    }

    pub(super) fn run(&mut self, s: &Stream) {
        loop {
            // Look for a preamble, whatever else is going on: a new one
            // during data is a new transmission.
            if self.pending.is_none() {
                self.pending = self.det.run(s);
            }
            if let Some(n0) = self.pending {
                if n0 + 15 * SLOT_N + 2 * SLOT_N > s.end() {
                    // D1 to C3 not here yet.
                    if self.run.is_none() {
                        return;
                    }
                } else {
                    self.pending = None;
                    if let Some((mode, count, dfe)) = self.read(s, n0) {
                        self.segment(n0, mode, count, dfe);
                    }
                    continue;
                }
            }
            let Some(r) = self.run.as_mut() else { return };
            let need = r.n0 + 2 * r.j as u64 + (NF - PRE) as u64 + 2;
            if s.end() < need {
                return;
            }
            Self::step(r, s, &mut self.notes, &mut self.shown);
            if r.done {
                self.finish();
            }
        }
    }

    /// A preamble segment read: the start of a transmission, or another
    /// segment of the one being run. Each later segment votes on where
    /// the data begins; one reading that disagrees with two that agree is
    /// ignored, but a first reading is not trusted against the next. A
    /// transmission whose data is being decoded gives way to a new one
    /// only on two segments that follow each other: data can, rarely,
    /// pass for one.
    fn segment(&mut self, n0: u64, mode: Mode, count: u32, dfe: Dfe) {
        let data = n0 + SEGMENT_N * (u64::from(count) + 1);
        if let Some(r) = self.run.as_mut() {
            let ours = r.n0 + 2 * r.data_at as u64;
            let same = mode == r.mode && n0 < ours + 2 * SEGMENT_N;
            if same && data.abs_diff(ours) < SLOT_N {
                r.votes += 1;
                return;
            }
            if same && r.votes >= 2 {
                return;
            }
            if same {
                // The first reading was wrong: start over from this one,
                // before any of its data has been decoded.
                let run = self.start(n0, mode, count, dfe);
                self.run = Some(run);
                return;
            }
            if r.j >= r.data_at {
                let follows = self.candidate.is_some_and(|(n, m, c)| {
                    m == mode && c == count + 1 && (n + SEGMENT_N).abs_diff(n0) < SLOT_N
                });
                if !follows {
                    self.candidate = Some((n0, mode, count));
                    return;
                }
            }
        }
        self.candidate = None;
        let run = self.start(n0, mode, count, dfe);
        self.finish();
        self.notes.push(Note::Mode(run.mode));
        self.run = Some(run);
    }

    /// Give up the current run, reporting what is pending.
    fn finish(&mut self) {
        if let Some(mut r) = self.run.take() {
            for d in &mut r.decoders {
                d.search.flush();
                for p in d.search.take() {
                    self.notes.push(Note::Dpdu(d.mode, p));
                }
            }
        }
    }

    /// Read D1 to C3 of the segment at `n0`, if they make sense: the mode,
    /// the count, and an equalizer trained on the segment's fixed slots.
    ///
    /// The equalizer, trained on slots 0 to 8, reads on into the slots
    /// that follow, so they are weighed coherently and with the echoes
    /// gathered in, not by one path's correlation alone: a count misread
    /// by one puts the data a segment off. Every word is BPSK on the
    /// scrambling sequence, so each symbol is decided on its own for the
    /// feedback while the slot's 32 are weighed together.
    fn read(&self, s: &Stream, n0: u64) -> Option<(Mode, u32, Dfe)> {
        let rad = self.det.turn(n0);
        let mut dfe = Dfe::new(rad);
        for j in 0..9 * SLOT {
            let x = s.slice(n0 + 2 * j as u64 - PRE as u64, NF)?;
            let y = dfe.filter(x);
            let k = j % SLOT;
            dfe.update(y, psk(8, (WORDS[FIXED[j / SLOT]][k % 8] + PRE_SCRAMBLE[k]) as usize % 8), true);
        }
        let mut read = [0u8; 6];
        for (i, v) in read.iter_mut().enumerate() {
            let mut corr = [0f32; 8];
            for k in 0..SLOT {
                let j = (9 + i) * SLOT + k;
                let x = s.slice(n0 + 2 * j as u64 - PRE as u64, NF)?;
                let y = dfe.filter(x);
                let scr = psk(8, PRE_SCRAMBLE[k] as usize);
                let z = y * scr.conj();
                for (w, c) in corr.iter_mut().enumerate() {
                    *c += if WORDS[w][k % 8] == 0 { z.re } else { -z.re };
                }
                dfe.update(y, if z.re >= 0.0 { scr } else { -scr }, false);
            }
            *v = (0..8).max_by(|&a, &b| corr[a].total_cmp(&corr[b])).unwrap() as u8;
        }
        let [d1, d2, c1, c2, c3, last] = read;
        if last != 0 || c1 < 4 || c2 < 4 || c3 < 4 {
            return None;
        }
        let count = u32::from(c1 - 4) << 4 | u32::from(c2 - 4) << 2 | u32::from(c3 - 4);
        let mode = Mode::from_d(d1, d2)?;
        let max = if mode.interleave == Interleave::Long { 23 } else { 2 };
        if count > max {
            return None;
        }
        Some((mode, count, dfe))
    }

    /// A run from the segment at `n0`, read as `mode` and `count`.
    fn start(&self, n0: u64, mode: Mode, count: u32, mut dfe: Dfe) -> Box<Run> {
        dfe.set_lambda(lambda_for(match mode.bps {
            4800 | 2400 => 8,
            1200 => 4,
            _ => 2,
        }));
        dfe.restart(self.det.turn(n0));
        let mut decoders = Vec::new();
        if mode.decodable() {
            decoders.push(Decoder::new(mode, mode.interleave));
            if mode.interleave == Interleave::Short && mode.bps < 4800 {
                decoders.push(Decoder::new(Mode { interleave: Interleave::Zero, ..mode }, Interleave::Zero));
            }
        }
        Box::new(Run {
            mode,
            n0,
            count,
            votes: 1,
            data_at: SEGMENT * (count as usize + 1),
            j: 0,
            dfe,
            var: 1.0,
            bad: 0,
            known_err: 0.0,
            known_n: 0,
            cons: mode.constellation(),
            decoders,
            soft: Vec::new(),
            zs: Vec::new(),
            walsh: Vec::new(),
            word_start: None,
            last_probe: 0.1,
            confident: confident_error(match mode.bps {
                4800 | 2400 => 8,
                1200 => 4,
                _ => 2,
            }),
            clock: Clock::default(),
            since_clock: 0,
            done: false,
        })
    }

    fn step(r: &mut Run, s: &Stream, notes: &mut Vec<Note>, shown: &mut VecDeque<C32>) {
        let at = r.n0 + 2 * r.j as u64 - PRE as u64;
        let Some(x) = s.slice(at, NF) else {
            r.done = true;
            return;
        };
        let y = r.dfe.filter(x);
        if r.j < r.data_at {
            let d = preamble_symbol(&r.mode, r.count, r.j);
            r.known(y, d);
            r.dfe.update(y, d, true);
        } else {
            let t = r.j - r.data_at;
            let scr = psk(8, SCRAMBLE[t % 160] as usize);
            let (u, k) = r.mode.frame();
            let f = t % (u + k);
            if f < u {
                if r.mode.bps == 75 && f == 0 {
                    r.word_start = Some(r.dfe.clone());
                }
                let z = y * scr.conj();
                let point = r.cons.points[r.cons.nearest(z)];
                let sure = (z - point).norm_sqr() < r.confident && r.mode.bps != 75;
                r.dfe.update(y, point * scr, sure);
                if shown.len() == super::s4285::SHOWN {
                    shown.pop_front();
                }
                shown.push_back(z);
                if r.mode.bps == 75 {
                    r.walsh.push((y, z));
                    if f + 1 == u {
                        Self::walsh_word(r, s, t / 32);
                    }
                } else {
                    r.zs.push(z);
                }
            } else {
                let d = psk(8, (probe_tribit(&r.mode, t, f - u) + SCRAMBLE[t % 160] as usize) % 8);
                r.known(y, d);
                r.dfe.update(y, d, true);
            }
            if f + 1 == u + k {
                Self::end_frame(r, notes);
            }
        }
        r.j += 1;
        // Follow the symbol clock, as in 4285, every 480 symbols.
        r.since_clock += 1;
        if r.since_clock >= 480 {
            r.since_clock = 0;
            let dir = r.clock.update(r.dfe.centroid(), 0.2);
            if dir != 0 {
                r.dfe.shift(dir);
                r.n0 = r.n0.wrapping_add_signed(i64::from(dir));
            }
        }
    }

    /// Read a 75 bps word: its correlation with each of the four Walsh
    /// words gives the two bits' soft values, and the word it most likely
    /// was stands in for the probes the mode does not send. The equalizer
    /// went through the word on symbol decisions alone, which in a fade
    /// at 75 bps's SNRs are often wrong; it is set back to the word's
    /// start and run through it again trained on the word, a decision
    /// made over 32 symbols at once.
    fn walsh_word(r: &mut Run, s: &Stream, set: usize) {
        let exc = usize::from(r.mode.exceptional(set));
        let sign = |v: usize, k: usize| if WORDS[MGD75[v] + 4 * exc][k % 8] == 0 { 1.0 } else { -1.0 };
        let mut c = [0f32; 4];
        for (v, cv) in c.iter_mut().enumerate() {
            *cv = r.walsh.iter().enumerate().map(|(k, (_, z))| z.re * sign(v, k)).sum();
        }
        let inv = 2.0 / r.var.max(1e-3);
        for bit in [1, 0] {
            let m0 = (0..4).filter(|v| (v >> bit) & 1 == 0).map(|v| c[v]).fold(f32::MIN, f32::max);
            let m1 = (0..4).filter(|v| (v >> bit) & 1 == 1).map(|v| c[v]).fold(f32::MIN, f32::max);
            r.soft.push(((m0 - m1) * inv).clamp(-50.0, 50.0));
        }
        let best = (0..4).max_by(|&a, &b| c[a].total_cmp(&c[b])).unwrap();
        let t0 = set * 32;
        r.walsh.clear();
        if let Some(d) = r.word_start.take() {
            r.dfe = d;
        }
        let j0 = r.data_at + t0;
        for k in 0..32 {
            let Some(x) = s.slice(r.n0 + 2 * (j0 + k) as u64 - PRE as u64, NF) else { return };
            let y = r.dfe.filter(x);
            let d = psk(8, SCRAMBLE[(t0 + k) % 160] as usize) * sign(best, k);
            r.known(y, d);
            r.dfe.update(y, d, true);
        }
    }

    fn end_frame(r: &mut Run, notes: &mut Vec<Note>) {
        // Soft bits scaled by the noise the probes either side of the data
        // saw: a fade is shorter than the 0.6 s interleaver, and the bits
        // from it must say they are unsure.
        let probe = if r.known_n > 0 { r.known_err / r.known_n as f32 } else { r.var };
        let var = (0.5 * (r.last_probe + probe)).max(0.2 * r.var).max(1e-3);
        r.last_probe = probe;
        for &z in &r.zs {
            r.cons.soft(z, var, &mut r.soft);
        }
        r.zs.clear();
        if r.known_n > 0 {
            let mse = r.known_err / r.known_n as f32;
            if mse > BAD_MSE {
                r.bad += 1;
                let (u, k) = r.mode.frame();
                if r.bad as usize * (u + k) >= LOST_SYMBOLS {
                    r.done = true;
                }
            } else {
                r.bad = 0;
            }
            r.known_err = 0.0;
            r.known_n = 0;
        }
        if r.decoders.is_empty() {
            r.soft.clear();
            return;
        }
        for d in &mut r.decoders {
            d.push(&r.soft);
            for p in d.search.take() {
                notes.push(Note::Dpdu(d.mode, p));
            }
        }
        r.soft.clear();
        // The decoder whose code checks says when the message ends.
        let best = r.decoders.iter().min_by(|a, b| a.ber().total_cmp(&b.ber()));
        if best.is_some_and(|d| d.eom) {
            notes.push(Note::Eom);
            r.done = true;
        }
    }
}

impl Run {
    fn known(&mut self, y: C32, d: C32) {
        let e = (d - y).norm_sqr();
        self.var += 0.01 * (e - self.var);
        self.known_err += e;
        self.known_n += 1;
    }
}

/// The tribit of preamble symbol `j` from the start of a segment whose
/// count is `count`.
fn preamble_symbol(mode: &Mode, count: u32, j: usize) -> C32 {
    let seg = j / SEGMENT;
    let c = count.saturating_sub(seg as u32);
    let slot = (j % SEGMENT) / SLOT;
    let k = j % SLOT;
    let word = match slot {
        0..=8 => FIXED[slot],
        9 => mode.d1 as usize,
        10 => mode.d2 as usize,
        11 => 4 + ((c >> 4) & 3) as usize,
        12 => 4 + ((c >> 2) & 3) as usize,
        13 => 4 + (c & 3) as usize,
        _ => 0,
    };
    psk(8, (WORDS[word][k % 8] + PRE_SCRAMBLE[k]) as usize % 8)
}

/// The known tribit (before scrambling) at position `i` of the probe in
/// the frame holding data symbol `t`: zero, but D1 and D2 in the last two
/// probes of an interleaver block.
fn probe_tribit(mode: &Mode, t: usize, i: usize) -> usize {
    let (u, k) = mode.frame();
    let block = mode.block_symbols();
    let frames = block / (u + k);
    let fi = (t % block) / (u + k);
    let word = if fi + 2 == frames {
        Some(mode.d1)
    } else if fi + 1 == frames {
        Some(mode.d2)
    } else {
        None
    };
    match word {
        Some(w) if i < 16 => WORDS[w as usize][i % 8] as usize,
        _ => 0,
    }
}

/// A mode by rate and interleaver, for the tests.
#[cfg(test)]
pub(super) fn test_mode(bps: u32, il: Interleave) -> Mode {
    let long = il == Interleave::Long;
    let (d1, d2) = match bps {
        4800 => (7, 6),
        2400 => (if long { 4 } else { 6 }, 4),
        1200 => (if long { 4 } else { 6 }, 5),
        600 => (if long { 4 } else { 6 }, 6),
        300 => (if long { 4 } else { 6 }, 7),
        150 => (if long { 5 } else { 7 }, 4),
        _ => (if long { 5 } else { 7 }, 5),
    };
    Mode { interleave: il, ..Mode::from_d(d1, d2).unwrap() }
}

/// The transmitter, for the tests: preamble, `bytes` (least significant
/// bit first), the end-of-message pattern, and the flush.
#[cfg(test)]
pub(super) fn transmit(mode: Mode, bytes: &[u8]) -> Vec<C32> {
    use super::fec::Encoder;
    let segments = if mode.interleave == Interleave::Long { 24 } else { 3 };
    let mut syms = Vec::new();
    for seg in 0..segments {
        let count = (segments - 1 - seg) as u32;
        for j in 0..SEGMENT {
            syms.push(preamble_symbol(&mode, count, j));
        }
    }
    let mut bits = super::s5066::bytes_to_bits(bytes, false);
    for i in (0..32).rev() {
        bits.push(((EOM >> i) & 1) as u8);
    }
    bits.extend(std::iter::repeat_n(0u8, 144));
    let mut coded = Vec::new();
    if mode.coded() {
        let mut enc = Encoder::default();
        for &b in &bits {
            let (c1, c2) = enc.push(b);
            for _ in 0..mode.rep() {
                coded.push(c1);
                coded.push(c2);
            }
        }
    } else {
        coded = bits;
    }
    let bps_sym = match mode.bps {
        4800 | 2400 => 3,
        1200 => 2,
        _ => 1,
    };
    let chan = match mode.matrix(mode.interleave) {
        Some((r, c)) => {
            let (load, fetch) = mode.steps();
            let il = BlockInterleaver::new(r, c, load, fetch);
            let n = il.len();
            coded.resize(coded.len().div_ceil(n) * n, 0);
            coded.chunks(n).flat_map(|b| il.interleave(b)).collect::<Vec<u8>>()
        }
        None => {
            let n = 1440 / mode.frame().0.max(1) * bps_sym;
            coded.resize(coded.len().div_ceil(n) * n, 0);
            coded
        }
    };
    let (u, k) = mode.frame();
    let mut ci = 0;
    let mut t = 0;
    if mode.bps == 75 {
        for (set, pair) in chan.chunks(2).enumerate() {
            let v = MGD75[(pair[0] << 1 | pair.get(1).copied().unwrap_or(0)) as usize];
            let w = v + 4 * usize::from(mode.exceptional(set));
            for k in 0..32 {
                syms.push(psk(8, (WORDS[w][k % 8] as usize + SCRAMBLE[t % 160] as usize) % 8));
                t += 1;
            }
        }
        return syms;
    }
    while ci < chan.len() {
        let f = t % (u + k);
        let tri = if f < u {
            let mut v = 0;
            for _ in 0..bps_sym {
                v = v << 1 | chan.get(ci).copied().unwrap_or(0) as usize;
                ci += 1;
            }
            match bps_sym {
                3 => MGD3[v],
                2 => MGD2[v],
                _ => 4 * v,
            }
        } else {
            probe_tribit(&mode, t, f - u)
        };
        syms.push(psk(8, (tri + SCRAMBLE[t % 160] as usize) % 8));
        t += 1;
    }
    // Finish the frame.
    while t % (u + k) != 0 {
        let f = t % (u + k);
        let tri = if f < u { 0 } else { probe_tribit(&mode, t, f - u) };
        syms.push(psk(8, (tri + SCRAMBLE[t % 160] as usize) % 8));
        t += 1;
    }
    syms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d_symbols_name_every_rate() {
        let m = Mode::from_d(6, 6).unwrap();
        assert_eq!((m.bps, m.interleave), (600, Interleave::Short));
        let m = Mode::from_d(4, 4).unwrap();
        assert_eq!((m.bps, m.interleave), (2400, Interleave::Long));
        assert!(Mode::from_d(0, 0).is_none());
    }

    #[test]
    fn block_sizes_fill_whole_frames() {
        for (d1, d2) in [(6, 4), (4, 4), (6, 5), (4, 5), (6, 6), (4, 6), (6, 7), (7, 4)] {
            let m = Mode::from_d(d1, d2).unwrap();
            let (u, k) = m.frame();
            let (r, c) = m.matrix(m.interleave).unwrap();
            let bits_per_sym = match m.bps {
                2400 => 3,
                1200 => 2,
                _ => 1,
            };
            let block = m.block_symbols();
            assert_eq!(block % (u + k), 0);
            assert_eq!(block / (u + k) * u * bits_per_sym, r * c, "{} bps", m.bps);
        }
    }

    #[test]
    fn count_symbols_follow_the_example() {
        // A count of 23 is 010111: C1 C2 C3 = 5, 5, 7.
        let m = Mode::from_d(4, 4).unwrap();
        let c = 23u32;
        let w = |slot: usize| {
            let j = slot * SLOT;
            (0..8).find(|&w| {
                (0..SLOT).all(|k| preamble_symbol(&m, c, j + k) == psk(8, (WORDS[w][k % 8] + PRE_SCRAMBLE[k]) as usize % 8))
            })
        };
        assert_eq!([w(11), w(12), w(13)], [Some(5), Some(5), Some(7)]);
    }
}
