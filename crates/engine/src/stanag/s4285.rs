//! STANAG 4285: 2400 Bd serial tone at 1800 Hz in 256-symbol frames, each
//! an 80-symbol BPSK sync (a 31-chip m-sequence, x^5 + x^2 + 1, sent
//! 31 + 31 + 18) then four 32-symbol data blocks with a 16-symbol probe
//! after each of the first three. Data and probes are scrambled by adding
//! a 9-stage sequence (x^9 + x^4 + 1, all ones at the start of each frame,
//! three bits a symbol) to the 8-PSK phase number, so the line looks like
//! 8-PSK whatever the user rate.
//!
//! Nothing in the signal says which of the nine user rates it carries,
//! nor whether the interleaver is short or long. The receiver finds the
//! sync, equalizes every frame (trained on the sync and probes, decision-
//! directed on data), and reads the constellation from the descrambled
//! data symbols: BPSK squares to a constant, QPSK does so at the fourth
//! power, 8-PSK at neither. Then every coded rate that constellation
//! allows is decoded at once, each with each interleaver setting, and the
//! one whose Viterbi decoder agrees with its own input is the mode. The
//! uncoded rates have no code to agree with; they are named only if a
//! STANAG 5066 D_PDU turns up in their bits.

use super::fec::{ConvDeinterleaver, Viterbi};
use super::modem::{C32, Clock, Constellation, Dfe, NF, PRE, Stream, confident_error, lambda_for, psk, rad_to_hz};
use super::s5066::{Dpdu, DpduSearch};
use std::collections::VecDeque;

pub(super) const BAUD: f32 = 2400.0;
pub(super) const FRAME: usize = 256;
const SYNC_LEN: usize = 80;
/// Samples a frame at two a symbol.
const FRAME_SAMPLES: u64 = 2 * FRAME as u64;

/// The sync: chips of the m-sequence, 1 sent as 180 degrees.
pub(super) const fn sync_chips() -> [u8; SYNC_LEN] {
    let mut c = [0u8; SYNC_LEN];
    let seed = [0u8, 1, 0, 1, 1];
    let mut i = 0;
    while i < SYNC_LEN {
        c[i] = if i < 5 { seed[i] } else { c[i - 5] ^ c[i - 3] };
        i += 1;
    }
    c
}

/// The scrambling sequence for a frame's 176 data and probe symbols.
pub(super) const fn scrambler() -> [u8; FRAME - SYNC_LEN] {
    let mut s = [0u8; FRAME - SYNC_LEN];
    let mut state: u16 = 0x1ff;
    let mut i = 0;
    while i < s.len() {
        s[i] = (state & 7) as u8;
        let mut k = 0;
        while k < 3 {
            let fb = (state ^ (state >> 4)) & 1;
            state = (state >> 1) | (fb << 8);
            k += 1;
        }
        i += 1;
    }
    s
}

const SYNC: [u8; SYNC_LEN] = sync_chips();
const SCRAMBLE: [u8; FRAME - SYNC_LEN] = scrambler();

/// What a symbol of the frame is.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Slot {
    Sync(usize),
    /// Data symbol number within the frame's 128, and its scrambler index.
    Data(usize, usize),
    Probe(usize),
}

pub(super) fn slot(p: usize) -> Slot {
    if p < SYNC_LEN {
        return Slot::Sync(p);
    }
    let q = p - SYNC_LEN;
    let (g, r) = (q / 48, q % 48);
    if g == 3 || r < 32 { Slot::Data(g * 32 + r, q) } else { Slot::Probe(q) }
}

/// 8-PSK phase numbers for each label: 4285's Gray maps.
pub(super) const BPSK: [usize; 2] = [0, 4];
pub(super) const QPSK: [usize; 4] = [0, 2, 6, 4];
pub(super) const PSK8: [usize; 8] = [1, 0, 2, 3, 6, 7, 5, 4];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Interleave {
    None,
    Short,
    Long,
}

impl Interleave {
    pub(super) fn label(self) -> &'static str {
        match self {
            Interleave::None => "none",
            Interleave::Short => "short",
            Interleave::Long => "long",
        }
    }
}

/// One user rate as the transmitter builds it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Mode {
    pub(super) bps: u32,
    /// Constellation order: 2, 4 or 8.
    pub(super) order: usize,
    pub(super) coded: bool,
    /// Times each coded pair is sent.
    pub(super) rep: usize,
    pub(super) interleave: Interleave,
}

impl Mode {
    /// Interleaver bits fed per data symbol: 2400 bps codes two bits into
    /// four and drops the fourth after interleaving.
    fn il_bits(&self) -> usize {
        match self.order {
            2 => 1,
            4 => 2,
            _ => 4,
        }
    }

    /// The convolutional interleaver's row increment.
    fn inc(&self) -> Option<usize> {
        let (s, l) = match self.order {
            2 => (1, 12),
            4 => (2, 24),
            _ => (4, 48),
        };
        match self.interleave {
            Interleave::None => None,
            Interleave::Short => Some(s),
            Interleave::Long => Some(l),
        }
    }

    pub(super) fn modulation(&self) -> &'static str {
        modulation(self.order)
    }
}

pub(super) fn modulation(order: usize) -> &'static str {
    match order {
        2 => "BPSK",
        4 => "QPSK",
        _ => "8-PSK",
    }
}

fn constellation(order: usize) -> Constellation {
    match order {
        2 => Constellation::from_phases(&BPSK, 1),
        4 => Constellation::from_phases(&QPSK, 2),
        _ => Constellation::from_phases(&PSK8, 3),
    }
}

/// The coded rates a constellation can carry, each with each interleaver.
fn coded_modes(order: usize) -> Vec<Mode> {
    let rates: &[(u32, usize)] = match order {
        2 => &[(600, 1), (300, 2), (150, 4), (75, 8)],
        4 => &[(1200, 1)],
        _ => &[(2400, 1)],
    };
    let mut v = Vec::new();
    for &(bps, rep) in rates {
        for il in [Interleave::Short, Interleave::Long, Interleave::None] {
            v.push(Mode { bps, order, coded: true, rep, interleave: il });
        }
    }
    v
}

fn uncoded_mode(order: usize) -> Mode {
    Mode { bps: 1200 * (order.trailing_zeros()), order, coded: false, rep: 1, interleave: Interleave::None }
}

/// The 80-symbol sync correlated in five pieces of 16 and the pieces' power
/// summed: a carrier 50 Hz off turns 120 degrees over a piece and costs
/// under 2 dB, where turning through 600 degrees over the whole sync would
/// cost all of it.
const PIECE: usize = 16;
/// The normalized correlation (1 for a clean sync, about 1/16 for noise)
/// that counts as a sync.
const DETECT: f32 = 0.25;
/// Successive syncs must land within this many samples of a frame apart:
/// a fading channel's strongest echo moves by a few symbols.
const PAIR_TOL: u64 = 12;
/// The sync is 31-periodic, so correlating it 31 symbols early or late
/// still matches three of its five pieces (a metric of 0.6). A peak is
/// final only once nothing stronger turns up within two periods of it.
const PEAK_GUARD: u64 = 4 * 31 + 8;

#[derive(Clone, Copy, Debug)]
struct Peak {
    n: u64,
    metric: f32,
    /// Sum over adjacent pieces of c[m+1] c[m]*: its angle is the carrier's
    /// turn per piece.
    turn: C32,
}

struct Detector {
    next: u64,
    best: Option<Peak>,
    peaks: VecDeque<Peak>,
    /// The strongest recent correlation, decaying, for the status scores.
    score: f32,
}

impl Detector {
    fn new() -> Self {
        Self { next: 0, best: None, peaks: VecDeque::new(), score: 0.0 }
    }

    fn metric(s: &Stream, n: u64) -> Option<(f32, C32)> {
        let x = s.slice(n, 2 * SYNC_LEN)?;
        let mut c = [C32::default(); SYNC_LEN / PIECE];
        let mut energy = 0.0;
        for k in 0..SYNC_LEN {
            let v = x[2 * k];
            energy += v.norm_sqr();
            if SYNC[k] == 0 {
                c[k / PIECE] += v;
            } else {
                c[k / PIECE] -= v;
            }
        }
        let p: f32 = c.iter().map(|v| v.norm_sqr()).sum();
        let mut turn = C32::default();
        for m in 0..c.len() - 1 {
            turn += c[m + 1] * c[m].conj();
        }
        Some((p / (PIECE as f32 * energy.max(1e-20)), turn))
    }

    /// Where the channel's energy is centered near a peak: echoes a few
    /// symbols apart each make a peak, and the equalizer's window should
    /// straddle them all rather than start from the strongest.
    fn center(s: &Stream, n: u64) -> u64 {
        let prof: Vec<(i64, f32)> = (-16i64..=16)
            .filter_map(|d| Self::metric(s, n.wrapping_add_signed(d)).map(|(m, _)| (d, m)))
            .collect();
        let max = prof.iter().fold(0f32, |a, p| a.max(p.1));
        let (mut w, mut m) = (0.0, 0.0);
        for &(d, v) in &prof {
            let x = (v - 0.3 * max).max(0.0);
            w += x;
            m += x * d as f32;
        }
        if w > 0.0 { n.wrapping_add_signed((m / w).round() as i64) } else { n }
    }

    /// Run over what has arrived; a pair of syncs a frame apart gives the
    /// frame start and a carrier offset in radians a symbol.
    fn run(&mut self, s: &Stream) -> Option<(u64, f32)> {
        self.next = self.next.max(s.start());
        let mut found = None;
        while self.next + 2 * SYNC_LEN as u64 <= s.end() {
            let n = self.next;
            self.next += 1;
            let (m, turn) = Self::metric(s, n).unwrap_or((0.0, C32::default()));
            self.score = (self.score * 0.9995).max(m);
            if m > DETECT && self.best.is_none_or(|b| m > b.metric) {
                self.best = Some(Peak { n, metric: m, turn });
            }
            if let Some(b) = self.best
                && n > b.n + PEAK_GUARD
            {
                self.best = None;
                if let Some(p) = self.peaks.iter().find(|p| p.n + FRAME_SAMPLES + PAIR_TOL >= b.n && p.n + FRAME_SAMPLES <= b.n + PAIR_TOL) {
                    let turn = p.turn + b.turn;
                    let rad = turn.arg() / PIECE as f32;
                    found = Some((Self::center(s, b.n), rad));
                }
                self.peaks.push_back(b);
                while self.peaks.front().is_some_and(|p| p.n + 2 * FRAME_SAMPLES < b.n) {
                    self.peaks.pop_front();
                }
                if found.is_some() {
                    break;
                }
            }
        }
        found
    }
}

/// One guess at the mode, decoded continuously.
struct Trial {
    mode: Mode,
    deint: Option<ConvDeinterleaver>,
    vit: Viterbi,
    pend: Vec<f32>,
    staged: Vec<f32>,
    bits: Vec<u8>,
    search: DpduSearch,
    frames: u32,
    /// Frames before its error rate means anything: the deinterleaver's
    /// delay and a few hundred bits more.
    warm: u32,
}

impl Trial {
    fn new(mode: Mode) -> Self {
        let lag_bits = mode.inc().map_or(0, |inc| 32 * 31 * inc);
        let per_frame = 128 * mode.il_bits();
        let warm = lag_bits.div_ceil(per_frame) as u32 + (800 * mode.rep).div_ceil(per_frame) as u32 + 2;
        Self {
            mode,
            deint: mode.inc().map(ConvDeinterleaver::new),
            vit: Viterbi::new(),
            pend: Vec::new(),
            staged: Vec::new(),
            bits: Vec::new(),
            search: DpduSearch::new(),
            frames: 0,
            warm,
        }
    }

    /// Soft bits of one frame's data symbols, `per` a symbol.
    fn frame(&mut self, soft: &[f32], per: usize) {
        self.frames += 1;
        if !self.mode.coded {
            self.bits.clear();
            self.bits.extend(soft.iter().map(|&v| u8::from(v < 0.0)));
            self.search.push_bits(&self.bits);
            return;
        }
        self.staged.clear();
        for sym in soft.chunks(per) {
            for &v in sym {
                self.stage(v);
            }
            if self.mode.order == 8 {
                // The punctured fourth bit: an erasure where it was dropped.
                self.stage(0.0);
            }
        }
        let rep = self.mode.rep;
        self.pend.extend_from_slice(&self.staged);
        let mut used = 0;
        while self.pend.len() - used >= 2 * rep {
            let g = &self.pend[used..used + 2 * rep];
            let a: f32 = g.iter().step_by(2).sum();
            let b: f32 = g.iter().skip(1).step_by(2).sum();
            self.vit.push(a, b);
            used += 2 * rep;
        }
        self.pend.drain(..used);
        self.bits.clear();
        self.vit.take(&mut self.bits);
        self.search.push_bits(&self.bits);
    }

    fn stage(&mut self, v: f32) {
        match &mut self.deint {
            Some(d) => d.push(v, &mut self.staged),
            None => self.staged.push(v),
        }
    }

    fn ber(&self) -> Option<f32> {
        if self.frames < self.warm { None } else { self.vit.ber() }
    }

    /// The decoded bits are neither all zeros nor all ones: a constant
    /// stream decodes cleanly under every guess and tells nothing.
    fn varied(&self) -> bool {
        (0.15..0.85).contains(&self.vit.ones())
    }
}

/// A coded mode is taken when its re-encoding disagreement is below this:
/// about half what the decoder reports on random input, 0.18 for the
/// rate 1/2 code and 0.12 for the rate 2/3 one 2400 bps punctures it to
/// (fewer bits to fit, so noise fits better).
fn decoding_ber(m: &Mode) -> f32 {
    if m.order == 8 { 0.055 } else { 0.09 }
}
/// The fewest frames (varied ones, not in a deep fade) whose likelihoods
/// are summed to choose the constellation.
const ORDER_FRAMES: u32 = 3;
/// The most: about five seconds, so a weak signal's evidence adds up but a
/// change of constellation is still followed.
const ORDER_WINDOW: usize = 48;
/// How far, in log-likelihood (nats), the best constellation must lead the
/// next before it is named; twice this to change one already named. A
/// strong signal clears it in a frame, one near 0 dB in a few seconds.
/// Near 0 dB QPSK and 8-PSK are hard to tell apart (QPSK's points are
/// among 8-PSK's), and a smaller margin was seen to name either on the
/// same real signal; this one waits until it is sure.
const ORDER_MARGIN: f32 = 20.0;
/// A frame whose known symbols show more noise than this (the signal
/// below the noise at the equalizer: a deep fade) is left out.
const ORDER_FADE_VAR: f32 = 1.5;
/// Bad syncs in a row (three seconds) before the lock is let go: a new
/// lock starts the deinterleavers over, which for the long setting costs
/// ten seconds, so a deep fade is waited out.
const LOST_FRAMES: u32 = 28;
/// A sync whose a-priori error is this large (signal at or below noise) is
/// bad.
const BAD_SYNC_MSE: f32 = 0.9;
/// Syncs with an error under this (signal 3 dB over noise at the
/// equalizer) confirm a lock, and this many of them are needed.
const GOOD_SYNC_MSE: f32 = 0.5;
const CONFIRM_FRAMES: u32 = 3;

/// Equalized symbols kept for the constellation display.
pub(super) const SHOWN: usize = 256;

struct Lock {
    frame_n: u64,
    dfe: Dfe,
    frames: u32,
    bad: u32,
    /// Frames whose sync was clearly there.
    good: u32,
    /// Mean squared a-priori error on known symbols: the noise variance
    /// at the equalizer's output.
    var: f32,
    /// The same for each data block, from the known symbols either side:
    /// fades are shorter than a frame, and soft bits from a fade must say
    /// they are unsure.
    block_var: [f32; 4],
    clock: Clock,
    order: Option<usize>,
    /// Per frame, the log-likelihood of the data under each constellation.
    order_ll: VecDeque<[f32; 3]>,
    order_votes: u32,
    trials: Vec<Trial>,
    mode: Option<Mode>,
    /// This frame's soft bits and descrambled data symbols.
    soft: Vec<f32>,
    zs: Vec<C32>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Note {
    Locked,
    Order(usize),
    Mode(Mode),
    Dpdu(Mode, Dpdu),
    Lost,
}

pub(super) struct Rx4285 {
    det: Detector,
    lock: Option<Lock>,
    notes: Vec<Note>,
    shown: VecDeque<C32>,
}

impl Rx4285 {
    pub(super) fn new() -> Self {
        Self { det: Detector::new(), lock: None, notes: Vec::new(), shown: VecDeque::with_capacity(SHOWN) }
    }

    /// Locked, and the lock has shown good syncs: a chance pairing of
    /// noise peaks never gets that far.
    pub(super) fn locked(&self) -> bool {
        self.lock.as_ref().is_some_and(|l| l.good >= CONFIRM_FRAMES)
    }

    /// The sync detector's recent best, 0 to 1.
    pub(super) fn score(&self) -> f32 {
        self.det.score
    }

    pub(super) fn order(&self) -> Option<usize> {
        self.lock.as_ref().and_then(|l| l.order)
    }

    pub(super) fn mode(&self) -> Option<Mode> {
        self.lock.as_ref().and_then(|l| l.mode)
    }

    pub(super) fn freq_error_hz(&self) -> Option<f32> {
        self.lock.as_ref().map(|l| rad_to_hz(l.dfe.freq(), BAUD))
    }

    /// Signal to noise in 3 kHz from the equalizer's error on known symbols.
    pub(super) fn snr_db(&self) -> Option<f32> {
        self.lock.as_ref().filter(|l| l.frames > 1).map(|l| 10.0 * (0.8 / l.var.max(1e-4)).log10())
    }

    pub(super) fn shown(&self) -> Vec<C32> {
        self.shown.iter().copied().collect()
    }

    pub(super) fn take_notes(&mut self) -> Vec<Note> {
        std::mem::take(&mut self.notes)
    }

    /// The earliest sample still needed.
    pub(super) fn needs_from(&self) -> u64 {
        match &self.lock {
            Some(l) => l.frame_n.saturating_sub(PRE as u64 + 4),
            None => self.det.next.saturating_sub(4 * FRAME_SAMPLES),
        }
    }

    pub(super) fn run(&mut self, s: &Stream) {
        loop {
            if self.lock.is_none() {
                let Some((n, rad)) = self.det.run(s) else { return };
                self.lock = Some(Lock {
                    frame_n: n,
                    dfe: Dfe::new(rad),
                    frames: 0,
                    bad: 0,
                    good: 0,
                    var: 1.0,
                    block_var: [1.0; 4],
                    clock: Clock::default(),
                    order: None,
                    order_ll: VecDeque::new(),
                    order_votes: 0,
                    trials: Vec::new(),
                    mode: None,
                    soft: Vec::new(),
                    zs: Vec::new(),
                });
                self.notes.push(Note::Locked);
            }
            let l = self.lock.as_mut().unwrap();
            let need = l.frame_n + 2 * (FRAME as u64 - 1) + (NF - PRE) as u64 + 2;
            if s.end() < need {
                return;
            }
            if !Self::frame(l, s, &mut self.notes, &mut self.shown) {
                let mut l = self.lock.take().unwrap();
                for t in &mut l.trials {
                    t.search.flush();
                    for d in t.search.take() {
                        self.notes.push(Note::Dpdu(t.mode, d));
                    }
                }
                self.notes.push(Note::Lost);
                // Hunt again from just after the last good frame.
                self.det = Detector::new();
                self.det.next = l.frame_n;
            }
        }
    }

    /// Equalize and decode one frame. False when the lock is lost.
    fn frame(l: &mut Lock, s: &Stream, notes: &mut Vec<Note>, shown: &mut VecDeque<C32>) -> bool {
        let first = l.frames == 0;
        let cons = constellation(l.order.unwrap_or(8));
        if first {
            // Train on the first sync twice: the taps start from nothing.
            let freq = l.dfe.freq();
            for (p, &chip) in SYNC.iter().enumerate() {
                let at = l.frame_n + 2 * p as u64 - PRE as u64;
                let Some(x) = s.slice(at, NF) else { return false };
                let y = l.dfe.filter(x);
                l.dfe.update(y, psk(8, 4 * chip as usize), true);
            }
            l.dfe.restart(freq);
        }
        let mut sync_err = 0.0;
        let mut known_err = 0.0;
        let mut known = 0;
        // Error on each known stretch: the sync and the three probes.
        let mut seg = [(0f32, 0u32); 4];
        let confident = confident_error(l.order.unwrap_or(8));
        l.zs.clear();
        for p in 0..FRAME {
            let at = l.frame_n + 2 * p as u64 - PRE as u64;
            let Some(x) = s.slice(at, NF) else { return false };
            let y = l.dfe.filter(x);
            match slot(p) {
                Slot::Data(_, q) => {
                    let scr = psk(8, SCRAMBLE[q] as usize);
                    let z = y * scr.conj();
                    l.zs.push(z);
                    let point = cons.points[cons.nearest(z)];
                    // Learn from a decision only when it is probably right:
                    // in a fade, wrong ones teach the taps the wrong channel.
                    let sure = (z - point).norm_sqr() < confident;
                    l.dfe.update(y, point * scr, sure);
                }
                known_slot => {
                    let d = match known_slot {
                        Slot::Sync(i) => psk(8, 4 * SYNC[i] as usize),
                        Slot::Probe(q) => psk(8, SCRAMBLE[q] as usize),
                        Slot::Data(..) => unreachable!(),
                    };
                    let e = (d - y).norm_sqr();
                    known_err += e;
                    known += 1;
                    let k = if p < SYNC_LEN { 0 } else { 1 + (p - SYNC_LEN) / 48 };
                    seg[k].0 += e;
                    seg[k].1 += 1;
                    if p < SYNC_LEN {
                        sync_err += e;
                    }
                    l.dfe.update(y, d, true);
                }
            }
        }
        for (k, v) in l.block_var.iter_mut().enumerate() {
            let m = |i: usize| seg[i].0 / seg[i].1.max(1) as f32;
            *v = if k < 3 { 0.5 * (m(k) + m(k + 1)) } else { m(3) };
        }
        l.frames += 1;
        let sync_mse = sync_err / SYNC_LEN as f32;
        l.var += 0.3 * (known_err / known as f32 - l.var);
        if !first && sync_mse > BAD_SYNC_MSE {
            l.bad += 1;
            if l.bad >= LOST_FRAMES {
                return false;
            }
        } else {
            l.bad = 0;
            if sync_mse < GOOD_SYNC_MSE {
                l.good += 1;
            }
        }
        for &z in &l.zs {
            if shown.len() == SHOWN {
                shown.pop_front();
            }
            shown.push_back(z);
        }

        // Follow the symbol clock: keep the taps' center of energy, averaged
        // over about a second, in the middle of the window.
        let dir = l.clock.update(l.dfe.centroid(), 0.1);
        if dir != 0 {
            l.dfe.shift(dir);
            l.frame_n = l.frame_n.wrapping_add_signed(i64::from(dir));
        }
        l.frame_n += FRAME_SAMPLES;

        if l.frames >= 2 && Self::judge_order(l) {
            notes.push(Note::Order(l.order.unwrap()));
        }
        let Some(order) = l.order else { return true };

        // Soft bits for this frame's data, then every trial.
        let cons = constellation(order);
        l.soft.clear();
        for (i, &z) in l.zs.iter().enumerate() {
            cons.soft(z, l.block_var[i / 32].max(0.2 * l.var).max(1e-3), &mut l.soft);
        }
        for t in &mut l.trials {
            t.frame(&l.soft, cons.bits);
            for d in t.search.take() {
                if l.mode != Some(t.mode) {
                    l.mode = Some(t.mode);
                    notes.push(Note::Mode(t.mode));
                }
                notes.push(Note::Dpdu(t.mode, d));
            }
        }
        // The mode is the coded trial that decodes, clearly.
        let mut ranked: Vec<(f32, Mode)> =
            l.trials.iter().filter_map(|t| t.ber().map(|b| (b, t.mode))).collect();
        ranked.sort_by(|a, b| a.0.total_cmp(&b.0));
        if let Some(&(b, m)) = ranked.first()
            && b < decoding_ber(&m)
            && l.trials.iter().find(|t| t.mode == m).is_some_and(|t| t.varied())
            && ranked.get(1).is_none_or(|r| r.0 > 2.0 * b)
            && l.mode != Some(m)
            && l.mode.is_none_or(|cur| cur.coded)
        {
            l.mode = Some(m);
            notes.push(Note::Mode(m));
        }
        true
    }

    /// Weigh the constellations against this frame's descrambled data:
    /// the likelihood of the symbols if each were uniform over BPSK, QPSK
    /// or 8-PSK points, given the noise the probes show. Data that sits
    /// on one point (an idle all-zero stream) says nothing about the
    /// order and is not counted. The frames' likelihoods add up until one
    /// constellation leads by [`ORDER_MARGIN`]: a noisy frame weighs less
    /// by itself, through the noise it shows, so a weak signal is judged
    /// from more of them rather than not at all. True when the order
    /// changes.
    fn judge_order(l: &mut Lock) -> bool {
        // A frame in a deep fade tells nothing and would only add noise.
        if l.block_var.iter().sum::<f32>() / 4.0 > ORDER_FADE_VAR {
            return false;
        }
        // An order whose mode decodes is not given up.
        if let Some(m) = l.mode
            && l.trials.iter().any(|t| t.mode == m && t.ber().is_some_and(|b| b < 0.15))
        {
            l.order_votes = 0;
            return false;
        }
        let mut hist = [0u32; 8];
        let mut ll = [0f32; 3];
        let sets: [&[usize]; 3] = [&BPSK, &QPSK, &PSK8];
        for (i, &z) in l.zs.iter().enumerate() {
            let inv = 1.0 / l.block_var[i / 32].max(0.01);
            let k = ((z.arg() / (std::f32::consts::PI / 4.0)).round() as i32).rem_euclid(8) as usize;
            hist[k] += 1;
            for (m, set) in sets.iter().enumerate() {
                let d: Vec<f32> = set.iter().map(|&p| -(z - psk(8, p)).norm_sqr() * inv).collect();
                let mx = d.iter().fold(f32::MIN, |a, &b| a.max(b));
                let sum: f32 = d.iter().map(|&v| (v - mx).exp()).sum();
                ll[m] += mx + (sum / set.len() as f32).ln();
            }
        }
        let n = l.zs.len().max(1) as u32;
        let top = *hist.iter().max().unwrap_or(&0);
        // Four fifths on one point: BPSK splits about evenly, an idle
        // stream does not split at all.
        if top * 5 > n * 4 {
            return false;
        }
        l.order_ll.push_back(ll);
        if l.order_ll.len() > ORDER_WINDOW {
            l.order_ll.pop_front();
        }
        if l.order_ll.len() < ORDER_FRAMES as usize {
            return false;
        }
        let mut tot = [0f32; 3];
        for f in &l.order_ll {
            for m in 0..3 {
                tot[m] += f[m];
            }
        }
        let mut rank = [0usize, 1, 2];
        rank.sort_by(|&a, &b| tot[b].total_cmp(&tot[a]));
        let (best, margin) = (rank[0], tot[rank[0]] - tot[rank[1]]);
        let order = [2, 4, 8][best];
        if l.order == Some(order) {
            l.order_votes = 0;
            return false;
        }
        let need = if l.order.is_none() { ORDER_MARGIN } else { 2.0 * ORDER_MARGIN };
        if margin < need {
            l.order_votes = 0;
            return false;
        }
        // Two frames running, so one odd frame does not decide.
        l.order_votes += 1;
        if l.order_votes < 2 {
            return false;
        }
        l.order_votes = 0;
        l.order = Some(order);
        l.dfe.set_lambda(lambda_for(order));
        l.mode = None;
        l.trials = coded_modes(order).into_iter().map(Trial::new).collect();
        l.trials.push(Trial::new(uncoded_mode(order)));
        true
    }
}

/// The transmitter, for the tests: `bytes` (least significant bit first)
/// as `frames` frames of `mode`, zeros after them.
#[cfg(test)]
pub(super) fn transmit(mode: Mode, bytes: &[u8], frames: usize) -> Vec<C32> {
    use super::fec::{ConvInterleaver, Encoder};
    let per_frame_syms = 128;
    let mut bits = super::s5066::bytes_to_bits(bytes, false);
    // Symbols' worth of channel bits.
    let mut chan: Vec<u8> = Vec::new();
    let mut enc = Encoder::default();
    let mut il = mode.inc().map(ConvInterleaver::new);
    let need = frames * per_frame_syms * mode.il_bits();
    let mut i = 0;
    while chan.len() < need {
        let b = bits.get(i).copied().unwrap_or(0);
        i += 1;
        if !mode.coded {
            chan.push(b);
            continue;
        }
        let (c1, c2) = enc.push(b);
        for _ in 0..mode.rep {
            for c in [c1, c2] {
                match &mut il {
                    Some(il) => il.push(c, &mut chan),
                    None => chan.push(c),
                }
            }
        }
    }
    bits.clear();
    let mut syms = Vec::with_capacity(frames * FRAME);
    let mut ci = 0;
    for _ in 0..frames {
        for p in 0..FRAME {
            match slot(p) {
                Slot::Sync(k) => syms.push(psk(8, 4 * SYNC[k] as usize)),
                Slot::Probe(q) => syms.push(psk(8, SCRAMBLE[q] as usize)),
                Slot::Data(_, q) => {
                    let phase = if mode.coded {
                        match mode.order {
                            2 => {
                                ci += 1;
                                BPSK[chan[ci - 1] as usize]
                            }
                            4 => {
                                let v = chan[ci] << 1 | chan[ci + 1];
                                ci += 2;
                                QPSK[v as usize]
                            }
                            _ => {
                                let v = chan[ci] << 2 | chan[ci + 1] << 1 | chan[ci + 2];
                                ci += 4;
                                PSK8[v as usize]
                            }
                        }
                    } else {
                        let n = mode.order.trailing_zeros() as usize;
                        let mut v = 0;
                        for k in 0..n {
                            v = v << 1 | chan[ci + k] as usize;
                        }
                        ci += n;
                        match mode.order {
                            2 => BPSK[v],
                            4 => QPSK[v],
                            _ => PSK8[v],
                        }
                    };
                    syms.push(psk(8, (phase + SCRAMBLE[q] as usize) % 8));
                }
            }
        }
    }
    syms
}

#[cfg(test)]
pub(super) fn mode(bps: u32, order: usize, interleave: Interleave) -> Mode {
    let coded = !(bps == 1200 && order == 2 || bps == 2400 && order == 4 || bps == 3600);
    let rep = if order == 2 && coded { (600 / bps) as usize } else { 1 };
    Mode { bps, order, coded, rep, interleave: if coded { interleave } else { Interleave::None } }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_is_the_published_m_sequence() {
        let s: String = SYNC.iter().map(|&b| char::from(b'0' + b)).collect();
        assert_eq!(&s[..31], "0101100111110001101110101000010");
        assert_eq!(&s[31..62], &s[..31]);
        assert_eq!(&s[62..], &s[..18]);
    }

    #[test]
    fn frame_layout() {
        let mut data = 0;
        let mut probe = 0;
        for p in 0..FRAME {
            match slot(p) {
                Slot::Data(i, _) => {
                    assert_eq!(i, data);
                    data += 1;
                }
                Slot::Probe(_) => probe += 1,
                Slot::Sync(_) => {}
            }
        }
        assert_eq!((data, probe), (128, 48));
        assert_eq!(slot(80 + 32), Slot::Probe(32));
        assert_eq!(slot(255), Slot::Data(127, 175));
    }

    #[test]
    fn scrambler_starts_all_ones() {
        // All ones in the register: the first symbol adds 7, and the
        // sequence does not repeat within a frame.
        assert_eq!(SCRAMBLE[0], 7);
        assert!(SCRAMBLE.windows(8).skip(1).all(|w| w != &SCRAMBLE[..8]));
    }
}
