//! RDS / RBDS: the 1187.5 bit/s data channel on broadcast FM's 57 kHz
//! subcarrier, which carries the station's name (PS), its RadioText (often
//! the song and artist), program type and clock.
//!
//! The chain, from the FM discriminator's multiplex (MPX) signal:
//!
//! 1. A PLL on the 19 kHz stereo pilot. RDS is locked to the pilot's third
//!    harmonic, so a locked pilot gives the 57 kHz carrier's frequency
//!    exactly; without one (a mono station) a free-running 57 kHz NCO stands
//!    in and the Costas loop below takes up the station's few hertz of error.
//! 2. Mix to baseband, low-pass and decimate to ~36 kHz.
//! 3. A Costas loop for the residual phase (BPSK, so ±180° stays ambiguous;
//!    the differential coding removes it).
//! 4. A biphase (Manchester) matched filter, an early-late bit clock, and a
//!    check that the clock is not sitting half a bit off (a biphase stream
//!    looks like its own negation shifted half a bit whenever two symbols in
//!    a row agree).
//! 5. Differential decoding, then block sync on the 26-bit blocks by their
//!    offset words, the (26,16) checkword, and burst correction up to 5 bits.
//! 6. Groups 0A/0B (PS, TA/TP), 2A/2B (RadioText), 3A + the RT+ ODA, 4A
//!    (clock), PTY from every group, read as RBDS (the North American PTY
//!    names and PI call letters).
//!
//! Nothing is reported that has not passed the checkword, and text is held
//! to more than that: a block that needed correcting is right only most of
//! the time, so every two characters of PS and RadioText must arrive twice
//! alike before they count, and a name is published only once all of it has
//! been confirmed since its last change. A group counts only once its PI has
//! been confirmed and it carries that PI, which keeps a false sync on noise
//! from feeding anything through.

use crate::classifier::DecodeEvent;
use serde::Serialize;
use std::collections::BTreeMap;
use std::f64::consts::TAU;

/// The data rate: 57 kHz / 48.
pub const BIT_RATE: f64 = 1187.5;
const PILOT_HZ: f64 = 19_000.0;
const CARRIER_HZ: f64 = 57_000.0;

/// Offset words added to each block's checkword, in block order A, B, C, D,
/// with C' (block 3 of a version-B group) last.
const OFFSET_A: u16 = 0x0FC;
const OFFSET_B: u16 = 0x198;
const OFFSET_C: u16 = 0x168;
const OFFSET_CP: u16 = 0x350;
const OFFSET_D: u16 = 0x1B4;
/// g(x) = x^10 + x^8 + x^7 + x^5 + x^4 + x^3 + 1.
const POLY: u32 = 0x5B9;

/// RT+ (RadioText Plus), the open data application that tags the title and
/// artist inside RadioText.
const AID_RTPLUS: u16 = 0x4BD7;

/// Seconds without a counted group before the station is forgotten. A short
/// fade keeps what was heard (marked unlocked); a long one means the text
/// may be out of date, so it goes.
const FORGET_AFTER_S: f64 = 15.0;

// ---------- the checkword ----------

/// The 10-bit checkword of a 16-bit information word, before its offset.
fn checkword(m: u16) -> u16 {
    let mut r = (m as u32) << 10;
    for i in (10..26).rev() {
        if r & (1 << i) != 0 {
            r ^= POLY << (i - 10);
        }
    }
    (r & 0x3FF) as u16
}

/// What a 26-bit word's check disagrees with, after its offset is removed.
fn syndrome(word: u32, offset: u16) -> u16 {
    checkword((word >> 10) as u16) ^ (word & 0x3FF) as u16 ^ offset
}

/// Syndrome → the burst of 5 bits or fewer that produces it. The code
/// corrects every such burst, so each has a syndrome of its own; any
/// syndrome that two bursts shared would be left out rather than guessed.
fn burst_table() -> Vec<u32> {
    let mut table = vec![0u32; 1024];
    let mut seen = vec![0u8; 1024];
    for len in 1..=5u32 {
        // A burst of `len` bits: both ends set, anything between.
        let inner = if len >= 2 { 1u32 << (len - 2) } else { 1 };
        for mid in 0..inner {
            let pattern = if len == 1 { 1 } else { (1 << (len - 1)) | (mid << 1) | 1 };
            for shift in 0..=(26 - len) {
                let e = pattern << shift;
                let s = syndrome(e, 0) as usize;
                if seen[s] == 0 || table[s] != e {
                    seen[s] = seen[s].saturating_add(1);
                }
                table[s] = e;
            }
        }
    }
    for s in 0..1024 {
        if seen[s] > 1 {
            table[s] = 0;
        }
    }
    table[0] = 0;
    table
}

// ---------- small f64 filters ----------

/// A biquad in f64: the RDS low-pass runs at about 1% of the sample rate,
/// where single precision starts to cost accuracy.
#[derive(Clone, Copy, Debug, Default)]
struct Bq {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Bq {
    fn lowpass(fc: f64, fs: f64, q: f64) -> Self {
        let w = TAU * fc / fs;
        let (s, c) = w.sin_cos();
        let alpha = s / (2.0 * q);
        let a0 = 1.0 + alpha;
        Self {
            b0: (1.0 - c) / 2.0 / a0,
            b1: (1.0 - c) / a0,
            b2: (1.0 - c) / 2.0 / a0,
            a1: -2.0 * c / a0,
            a2: (1.0 - alpha) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn process(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// Fourth-order Butterworth low-pass as two sections.
fn butter4(fc: f64, fs: f64) -> [Bq; 2] {
    [Bq::lowpass(fc, fs, 0.541_196_1), Bq::lowpass(fc, fs, 1.306_563)]
}

// ---------- the demodulator: MPX in, data bits out ----------

/// A 19 kHz PLL updated once per short block, with a lock detector.
#[derive(Clone, Debug)]
struct PilotPll {
    /// Current phasor (cos, sin) and its per-sample step.
    c: f64,
    s: f64,
    step_c: f64,
    step_s: f64,
    /// Radians per sample.
    freq: f64,
    center: f64,
    fs: f64,
    /// Correlation accumulated over the current block.
    acc_i: f64,
    acc_q: f64,
    n: usize,
    block: usize,
    kp: f64,
    ki: f64,
    /// Smoothed cos(phase error): 1 locked, ~0 on noise.
    lock: f64,
    locked: bool,
}

impl PilotPll {
    fn new(fs: f64) -> Self {
        // A 250 µs block: four and three-quarter pilot cycles, nine and a half
        // of the 38 kHz product, which the sum all but cancels.
        let block = (fs / 4_000.0).round().max(4.0) as usize;
        let t = block as f64 / fs;
        let wn = TAU * 8.0 * t;
        let center = TAU * PILOT_HZ / fs;
        let mut p = Self {
            c: 1.0,
            s: 0.0,
            step_c: 1.0,
            step_s: 0.0,
            freq: center,
            center,
            fs,
            acc_i: 0.0,
            acc_q: 0.0,
            n: 0,
            block,
            kp: 2.0 * 0.707 * wn,
            ki: wn * wn,
            lock: 0.0,
            locked: false,
        };
        p.set_step();
        p
    }

    fn set_step(&mut self) {
        let (s, c) = self.freq.sin_cos();
        self.step_c = c;
        self.step_s = s;
    }

    /// Advance one sample with input `x`; returns the pilot phasor in use for
    /// this sample.
    #[inline]
    fn step(&mut self, x: f64) -> (f64, f64) {
        let (c, s) = (self.c, self.s);
        self.acc_i += x * c;
        self.acc_q -= x * s;
        let nc = c * self.step_c - s * self.step_s;
        let ns = s * self.step_c + c * self.step_s;
        self.c = nc;
        self.s = ns;
        self.n += 1;
        if self.n >= self.block {
            self.update();
        }
        (c, s)
    }

    fn update(&mut self) {
        let mag = self.acc_i.hypot(self.acc_q);
        if mag > 0.0 {
            let err = self.acc_q.atan2(self.acc_i);
            // The loop's own integrator, held near 19 kHz so that following
            // noise cannot walk it somewhere a real pilot would not be.
            let max = TAU * 20.0 / self.fs;
            let k = self.block as f64;
            self.freq = (self.freq + self.ki * err / k).clamp(self.center - max, self.center + max);
            // Phase correction applied by rotating the phasor.
            let (ps, pc) = (self.kp * err).sin_cos();
            let (c, s) = (self.c, self.s);
            self.c = c * pc - s * ps;
            self.s = s * pc + c * ps;
            // Smoothed over about a quarter second.
            self.lock += (self.acc_i / mag - self.lock) * 0.004;
            if self.locked && self.lock < 0.5 {
                self.locked = false;
            } else if !self.locked && self.lock > 0.8 {
                self.locked = true;
            }
        }
        // Keep the phasor on the unit circle.
        let r = self.c.hypot(self.s);
        if r > 0.0 {
            self.c /= r;
            self.s /= r;
        }
        self.set_step();
        self.acc_i = 0.0;
        self.acc_q = 0.0;
        self.n = 0;
    }
}

/// From the MPX signal to differentially decoded data bits.
#[derive(Clone, Debug)]
struct Demod {
    pilot: PilotPll,
    /// The free-running 57 kHz phasor for a station without a pilot.
    free_c: f64,
    free_s: f64,
    free_step_c: f64,
    free_step_s: f64,
    free_n: usize,
    lp_i: [Bq; 2],
    lp_q: [Bq; 2],
    decim: usize,
    decim_n: usize,
    fs_d: f64,
    /// Costas loop: phase, frequency (radians per decimated sample).
    ph: f64,
    fr: f64,
    ck_p: f64,
    ck_i: f64,
    /// Baseband power, smoothed, for normalizing.
    pow: f64,
    pow_k: f64,
    /// Matched filter: the last `2h` samples and their two half sums.
    h: usize,
    ring: Vec<f64>,
    ring_at: usize,
    sum_a: f64,
    sum_b: f64,
    /// Matched-filter outputs, newest last, for the timing decisions.
    hist: Vec<f64>,
    hist_at: usize,
    /// Bit clock.
    sps: f64,
    clock: f64,
    /// Average |y| on the clock and half a bit away from it.
    on_avg: f64,
    off_avg: f64,
    last_sym: bool,
    bits: Vec<bool>,
    using_pilot: bool,
}

impl Demod {
    fn new(fs: f64) -> Self {
        let decim = (fs / 38_000.0).round().max(1.0) as usize;
        let fs_d = fs / decim as f64;
        let sps = fs_d / BIT_RATE;
        let h = (sps / 2.0).round().max(2.0) as usize;
        let wn = TAU * 25.0 / fs_d;
        let (fsn, fcs) = (TAU * CARRIER_HZ / fs).sin_cos();
        Self {
            pilot: PilotPll::new(fs),
            free_c: 1.0,
            free_s: 0.0,
            free_step_c: fcs,
            free_step_s: fsn,
            free_n: 0,
            // 3.2 kHz keeps the ±2.4 kHz RDS band and the next mixer
            // products out; the matched filter does the rest.
            lp_i: butter4(3_200.0, fs),
            lp_q: butter4(3_200.0, fs),
            decim,
            decim_n: 0,
            fs_d,
            ph: 0.0,
            fr: 0.0,
            ck_p: 2.0 * 0.707 * wn,
            ck_i: wn * wn,
            pow: 0.0,
            pow_k: 1.0 / (0.05 * fs_d),
            h,
            ring: vec![0.0; 2 * h],
            ring_at: 0,
            sum_a: 0.0,
            sum_b: 0.0,
            hist: vec![0.0; sps.ceil() as usize + 4],
            hist_at: 0,
            sps,
            clock: 0.0,
            on_avg: 0.0,
            off_avg: 0.0,
            last_sym: false,
            bits: Vec::new(),
            using_pilot: false,
        }
    }

    fn process(&mut self, mpx: &[f32]) {
        for &x in mpx {
            let x = x as f64;
            let (pc, ps) = self.pilot.step(x);
            // The carrier: three times the pilot's phase when it is locked,
            // the free-running oscillator when it is not.
            let (cc, cs) = if self.pilot.locked {
                (4.0 * pc * pc * pc - 3.0 * pc, 3.0 * ps - 4.0 * ps * ps * ps)
            } else {
                (self.free_c, self.free_s)
            };
            let (fc, fsn) = (self.free_c, self.free_s);
            self.free_c = fc * self.free_step_c - fsn * self.free_step_s;
            self.free_s = fsn * self.free_step_c + fc * self.free_step_s;
            self.free_n += 1;
            if self.free_n >= 4096 {
                self.free_n = 0;
                let r = self.free_c.hypot(self.free_s);
                self.free_c /= r;
                self.free_s /= r;
            }
            let mut i = x * cc;
            let mut q = -x * cs;
            for f in &mut self.lp_i {
                i = f.process(i);
            }
            for f in &mut self.lp_q {
                q = f.process(q);
            }
            self.decim_n += 1;
            if self.decim_n >= self.decim {
                self.decim_n = 0;
                if self.using_pilot != self.pilot.locked {
                    // The carrier's source changed and its phase with it;
                    // the Costas loop starts over from here.
                    self.using_pilot = self.pilot.locked;
                    self.fr = 0.0;
                }
                self.baseband(i, q);
            }
        }
    }

    fn baseband(&mut self, i: f64, q: f64) {
        self.pow += (i * i + q * q - self.pow) * self.pow_k;
        let norm = if self.pow > 0.0 { 1.0 / self.pow.sqrt() } else { 0.0 };
        let (s, c) = self.ph.sin_cos();
        let re = (i * c + q * s) * norm;
        let im = (q * c - i * s) * norm;
        // BPSK Costas error, bounded so a burst cannot throw the loop.
        let err = (re.signum() * im).clamp(-1.0, 1.0);
        self.fr = (self.fr + self.ck_i * err).clamp(-TAU * 30.0 / self.fs_d, TAU * 30.0 / self.fs_d);
        self.ph = (self.ph + self.fr + self.ck_p * err).rem_euclid(TAU);

        // Matched filter for one biphase symbol: the older half-bit minus the
        // newer one, as running sums over a ring of 2h samples.
        let h = self.h;
        let oldest = self.ring[self.ring_at];
        let middle = self.ring[(self.ring_at + h) % (2 * h)];
        self.sum_a += middle - oldest;
        self.sum_b += re - middle;
        self.ring[self.ring_at] = re;
        self.ring_at = (self.ring_at + 1) % (2 * h);
        let y = self.sum_a - self.sum_b;
        let n = self.hist.len();
        self.hist[self.hist_at] = y;
        self.hist_at = (self.hist_at + 1) % n;

        self.clock += 1.0 / self.sps;
        if self.clock >= 1.0 {
            self.clock -= 1.0;
            self.on_bit();
        }
    }

    /// `k` samples before the newest matched-filter output.
    fn y_back(&self, k: usize) -> f64 {
        let n = self.hist.len();
        self.hist[(self.hist_at + n - 1 - k.min(n - 1)) % n]
    }

    fn on_bit(&mut self) {
        // The decision is made a little in the past so a late sample exists.
        let d = (self.sps / 8.0).round().max(1.0) as usize;
        let half = (self.sps / 2.0).round() as usize;
        let late = self.y_back(0).abs();
        let prompt = self.y_back(d);
        let early = self.y_back(2 * d).abs();
        let off = self.y_back(d + half).abs();
        // Early-late: a later peak means the clock is early; pull it back.
        let sum = late + early;
        if sum > 0.0 {
            self.clock -= 0.03 * (late - early) / sum;
        }
        self.on_avg += (prompt.abs() - self.on_avg) * 0.05;
        self.off_avg += (off - self.off_avg) * 0.05;
        if self.off_avg > 1.25 * self.on_avg && self.on_avg + self.off_avg > 0.0 {
            // Locked half a bit out: move to the other half.
            self.clock += 0.5;
            std::mem::swap(&mut self.on_avg, &mut self.off_avg);
        }
        let sym = prompt > 0.0;
        self.bits.push(sym != self.last_sym);
        self.last_sym = sym;
    }

    fn reset(&mut self) {
        *self = Self::new(self.pilot.fs);
    }
}

// ---------- block sync ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlockOk {
    Exact,
    Corrected,
}

#[derive(Clone, Copy, Debug)]
struct Block {
    data: u16,
    ok: BlockOk,
    /// Block 3 carried offset C' (a version-B group).
    c_prime: bool,
}

#[derive(Clone, Debug)]
struct Sync {
    reg: u32,
    bit_count: u64,
    synced: bool,
    /// Bits since the last block boundary, when synced.
    bits_in: u32,
    expect: usize,
    /// Recent exact offset hits while searching: (bit count, block index).
    hits: Vec<(u64, usize)>,
    /// The last 50 blocks: true where the block could not be read.
    recent: Vec<bool>,
    recent_at: usize,
    /// Blocks since one passed its checkword with no correction. Noise
    /// "corrects" about a third of the time, but is exact only one block
    /// in a thousand, so this is what tells a held sync from a lost one.
    since_exact: u32,
    /// Bumped each time sync is acquired, so a group can tell it belongs
    /// to the same run as the last.
    run: u64,
    group: [Option<Block>; 4],
    table: Vec<u32>,
}

impl Sync {
    fn new() -> Self {
        Self {
            reg: 0,
            bit_count: 0,
            synced: false,
            bits_in: 0,
            expect: 0,
            hits: Vec::new(),
            recent: Vec::new(),
            recent_at: 0,
            since_exact: 0,
            run: 0,
            group: [None; 4],
            table: burst_table(),
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    fn exact_index(word: u32) -> Option<(usize, bool)> {
        [OFFSET_A, OFFSET_B, OFFSET_C, OFFSET_CP, OFFSET_D]
            .iter()
            .position(|&o| syndrome(word, o) == 0)
            .map(|k| match k {
                0 => (0, false),
                1 => (1, false),
                2 => (2, false),
                3 => (2, true),
                _ => (3, false),
            })
    }

    /// Fraction of the last 50 blocks that could not be read.
    fn bler(&self) -> f32 {
        if self.recent.is_empty() {
            return 1.0;
        }
        self.recent.iter().filter(|&&b| b).count() as f32 / self.recent.len() as f32
    }

    fn note(&mut self, bad: bool) {
        if self.recent.len() < 50 {
            self.recent.push(bad);
        } else {
            self.recent[self.recent_at] = bad;
            self.recent_at = (self.recent_at + 1) % 50;
        }
    }

    /// One data bit in; a whole group out when its fourth block has been
    /// read (blocks that failed are `None`).
    fn push(&mut self, bit: bool) -> Option<[Option<Block>; 4]> {
        self.reg = ((self.reg << 1) | bit as u32) & 0x3FF_FFFF;
        self.bit_count += 1;
        if !self.synced {
            if self.bit_count < 26 {
                return None;
            }
            if let Some((idx, _)) = Self::exact_index(self.reg) {
                let now = self.bit_count;
                self.hits.retain(|&(t, _)| now - t <= 26 * 6);
                let agree = self.hits.iter().any(|&(t, j)| {
                    let dist = now - t;
                    dist % 26 == 0 && (j + (dist / 26) as usize) % 4 == idx
                });
                self.hits.push((now, idx));
                if agree {
                    self.synced = true;
                    self.run += 1;
                    self.since_exact = 0;
                    self.bits_in = 0;
                    self.expect = (idx + 1) % 4;
                    // The block history carries across: it describes the
                    // channel, and starting it over would read 100% lost.
                    self.group = [None; 4];
                    self.hits.clear();
                }
            }
            return None;
        }
        self.bits_in += 1;
        if self.bits_in < 26 {
            return None;
        }
        self.bits_in = 0;
        let idx = self.expect;
        self.expect = (idx + 1) % 4;
        let word = self.reg;
        let offsets: &[(u16, bool)] = match idx {
            0 => &[(OFFSET_A, false)],
            1 => &[(OFFSET_B, false)],
            2 => &[(OFFSET_C, false), (OFFSET_CP, true)],
            _ => &[(OFFSET_D, false)],
        };
        let mut block = None;
        for &(o, cp) in offsets {
            if syndrome(word, o) == 0 {
                block = Some(Block { data: (word >> 10) as u16, ok: BlockOk::Exact, c_prime: cp });
                break;
            }
        }
        if block.is_none() {
            for &(o, cp) in offsets {
                let e = self.table[syndrome(word, o) as usize];
                if e != 0 {
                    let fixed = word ^ e;
                    block = Some(Block { data: (fixed >> 10) as u16, ok: BlockOk::Corrected, c_prime: cp });
                    break;
                }
            }
        }
        self.note(block.is_none());
        self.group[idx] = block;
        if block.is_some_and(|b| b.ok == BlockOk::Exact) {
            self.since_exact = 0;
        } else {
            self.since_exact += 1;
        }
        // Thirty blocks (two thirds of a second) without one exact: the sync
        // was false, has slipped a bit, or the signal has gone.
        if self.since_exact >= 30 {
            self.synced = false;
            self.hits.clear();
            self.group = [None; 4];
            return None;
        }
        if idx == 3 {
            let g = self.group;
            self.group = [None; 4];
            return Some(g);
        }
        None
    }
}

// ---------- confirmed text ----------

/// Two characters of a text, held until they have been heard twice alike.
#[derive(Clone, Copy, Debug, Default)]
struct Pair {
    /// The last few readings that did not match: on a weak signal a
    /// misread often falls between two good ones, and a single slot would
    /// let it push the first good one out before the second arrived.
    cand: [Option<[u8; 2]>; 3],
    conf: Option<[u8; 2]>,
    /// Confirmed again since some other part of the text last changed.
    fresh: bool,
}

#[derive(Clone, Debug)]
struct Text {
    pairs: Vec<Pair>,
}

impl Text {
    fn new(n: usize) -> Self {
        Self { pairs: vec![Pair::default(); n] }
    }

    fn clear(&mut self) {
        for p in &mut self.pairs {
            *p = Pair::default();
        }
    }

    /// Take two characters for pair `k`; true if a confirmed pair changed.
    /// `exact` is a reading whose blocks needed no correction: when one of
    /// those disagrees with what is held, the station is sending something
    /// new, and nothing held counts as current until it is heard again. On
    /// a PS that scrolls, that is what keeps two frames from being spliced.
    fn put(&mut self, k: usize, v: [u8; 2], exact: bool) -> bool {
        if k >= self.pairs.len() {
            return false;
        }
        if exact && self.pairs[k].conf.is_some_and(|c| c != v) {
            for q in &mut self.pairs {
                q.fresh = false;
            }
        }
        let p = &mut self.pairs[k];
        if p.conf == Some(v) {
            p.fresh = true;
            return false;
        }
        if p.cand.contains(&Some(v)) {
            let changed = p.conf.is_some();
            p.conf = Some(v);
            p.cand = [None; 3];
            p.fresh = true;
            if changed {
                for (j, q) in self.pairs.iter_mut().enumerate() {
                    if j != k {
                        q.fresh = false;
                    }
                }
            }
            return changed;
        }
        p.cand.rotate_right(1);
        p.cand[0] = Some(v);
        false
    }

    /// The text, once every pair up to its end (a carriage return, or the
    /// last pair) is confirmed and fresh.
    fn complete(&self) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(self.pairs.len() * 2);
        for p in &self.pairs {
            if !p.fresh {
                return None;
            }
            let v = p.conf?;
            for c in v {
                if c == 0x0D {
                    return Some(out);
                }
                out.push(c);
            }
        }
        Some(out)
    }

    /// True once any pair has been confirmed.
    fn any(&self) -> bool {
        self.pairs.iter().any(|p| p.conf.is_some())
    }
}

/// One RDS character (the basic RDS table, as US stations use it).
/// Characters outside it come out as U+FFFD rather than as a guess.
fn rds_char(c: u8) -> char {
    const ROW8: [char; 16] = ['á', 'à', 'é', 'è', 'í', 'ì', 'ó', 'ò', 'ú', 'ù', 'Ñ', 'Ç', 'Ş', 'ß', '¡', 'Ĳ'];
    const ROW9: [char; 16] = ['â', 'ä', 'ê', 'ë', 'î', 'ï', 'ô', 'ö', 'û', 'ü', 'ñ', 'ç', 'ş', 'ğ', 'ı', 'ĳ'];
    match c {
        0x20..=0x7D => c as char,
        0x80..=0x8F => ROW8[(c - 0x80) as usize],
        0x90..=0x9F => ROW9[(c - 0x90) as usize],
        _ => '\u{FFFD}',
    }
}

fn to_string(bytes: &[u8]) -> String {
    bytes.iter().map(|&c| rds_char(c)).collect::<String>().trim().to_string()
}

/// The North American (RBDS) program type names.
pub fn pty_name(pty: u8) -> &'static str {
    const NAMES: [&str; 32] = [
        "None",
        "News",
        "Information",
        "Sports",
        "Talk",
        "Rock",
        "Classic Rock",
        "Adult Hits",
        "Soft Rock",
        "Top 40",
        "Country",
        "Oldies",
        "Soft",
        "Nostalgia",
        "Jazz",
        "Classical",
        "Rhythm and Blues",
        "Soft Rhythm and Blues",
        "Language",
        "Religious Music",
        "Religious Talk",
        "Personality",
        "Public",
        "College",
        "Spanish Talk",
        "Spanish Music",
        "Hip Hop",
        "Unassigned",
        "Unassigned",
        "Weather",
        "Emergency Test",
        "Emergency",
    ];
    NAMES[(pty & 31) as usize]
}

/// Call letters from a PI code by the RBDS mapping, for codes in its K/W
/// range. Stations that kept an old PI after a call change will not match
/// their current call, so this is labeled as derived wherever it is shown.
pub fn pi_call(pi: u16) -> Option<String> {
    let pi = pi as u32;
    let (prefix, mut n) = match pi {
        4096..=21671 => ('K', pi - 4096),
        21672..=39247 => ('W', pi - 21672),
        _ => return None,
    };
    let mut s = [0u8; 3];
    for c in s.iter_mut().rev() {
        *c = b'A' + (n % 26) as u8;
        n /= 26;
    }
    Some(format!("{prefix}{}", std::str::from_utf8(&s).ok()?))
}

/// What the station is sending, as far as it has been confirmed.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RdsStation {
    /// Program identification, as four hex digits.
    pub pi: String,
    /// Call letters derived from the PI by the RBDS mapping, when it is in
    /// that range.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pi_call: Option<String>,
    /// The 8-character program service name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ps: Option<String>,
    /// The station scrolls words through its PS, so `ps` is a word of a
    /// longer message rather than its name.
    pub ps_dynamic: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pty: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pty_name: Option<&'static str>,
    pub tp: bool,
    pub ta: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rt: Option<String>,
    /// From RT+ tags, while the station marks an item as running.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    /// The station's clock (group 4A), as sent: UTC, and its local offset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock_utc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock_offset_min: Option<i32>,
    /// Block sync held now.
    pub locked: bool,
    /// Whether the 57 kHz carrier is taken from the 19 kHz pilot.
    pub pilot: bool,
    /// Share of the last 50 blocks that could not be read, 0..1.
    pub bler: f32,
    /// Unix ms when something above (other than `bler`) last changed; the
    /// server stamps it.
    pub updated_ms: u64,
}

/// A change worth one line in the decode log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RdsChange {
    Ps,
    Rt,
    RtPlus,
}

/// What has been heard from the station on the channel, cleared when
/// another PI takes over or the station has been gone a while.
#[derive(Clone, Debug)]
struct Heard {
    pi_cand: Option<u16>,
    pi_hits: u8,
    pi: Option<u16>,
    pty_cand: Option<u8>,
    pty: Option<u8>,
    /// Flags from block B. A block B that needed correcting is wrong a good
    /// part of the time, so a flag changes only once two groups in a row
    /// agree on the new value; a lone misread must not wipe a RadioText.
    tp_cand: Option<bool>,
    tp: bool,
    ta_cand: Option<bool>,
    ta: bool,
    ps: Text,
    ps_out: Option<String>,
    /// Names published in the last minute, by sample time: a station that
    /// scrolls words through its PS sends a new one every few seconds.
    ps_seen: Vec<(u64, String)>,
    ps_dynamic: bool,
    rt: Text,
    rt_ab: Option<bool>,
    /// 2B (32 characters) rather than 2A (64).
    rt_b: Option<bool>,
    /// A new (A/B flag, version) heard once, waiting for a second group.
    rt_flip: Option<(bool, bool)>,
    rt_out: Option<String>,
    rt_raw: Option<Vec<u8>>,
    /// The group type RT+ is carried in, from group 3A.
    rtplus_group: Option<(u8, bool)>,
    rtplus_cand: Option<(u16, u16, u16)>,
    rtplus_toggle: Option<bool>,
    /// (content type, start, length) of the confirmed tags, and whether an
    /// item is running.
    tags: Vec<(u8, usize, usize)>,
    running: bool,
    title: Option<String>,
    artist: Option<String>,
    clock: Option<(String, i32)>,
    /// The sync run in which a readable PI last matched this station's.
    pi_run: Option<u64>,
}

impl Heard {
    fn new() -> Self {
        Self {
            pi_cand: None,
            pi_hits: 0,
            pi: None,
            pty_cand: None,
            pty: None,
            tp_cand: None,
            tp: false,
            ta_cand: None,
            ta: false,
            ps: Text::new(4),
            ps_out: None,
            ps_seen: Vec::new(),
            ps_dynamic: false,
            rt: Text::new(32),
            rt_ab: None,
            rt_b: None,
            rt_flip: None,
            rt_out: None,
            rt_raw: None,
            rtplus_group: None,
            rtplus_cand: None,
            rtplus_toggle: None,
            tags: Vec::new(),
            running: false,
            title: None,
            artist: None,
            clock: None,
            pi_run: None,
        }
    }
}

/// The whole decoder: MPX samples in, the station's data out.
#[derive(Clone, Debug)]
pub struct RdsDecoder {
    fs: f64,
    demod: Demod,
    sync: Sync,
    h: Heard,
    /// Samples since a group was last counted.
    quiet: u64,
    /// Samples taken in all.
    samples: u64,
    generation: u64,
    changes: Vec<RdsChange>,
}

impl RdsDecoder {
    /// `fs` is the MPX sample rate: the FM discriminator's, at least 128 kHz.
    pub fn new(fs: f64) -> Self {
        Self { fs, demod: Demod::new(fs), sync: Sync::new(), h: Heard::new(), quiet: 0, samples: 0, generation: 0, changes: Vec::new() }
    }

    /// Restart bit and block sync after a break in the samples, keeping what
    /// the station has already been heard to send.
    pub fn reset_sync(&mut self) {
        self.demod.reset();
        self.sync.reset();
    }

    fn forget_station(&mut self) {
        if self.h.pi.is_some() {
            self.generation += 1;
        }
        self.h = Heard::new();
    }

    /// Feed MPX samples (the discriminator's output before de-emphasis, in
    /// any consistent unit).
    pub fn process(&mut self, mpx: &[f32]) {
        self.demod.process(mpx);
        let was_locked = self.sync.synced;
        let bits = std::mem::take(&mut self.demod.bits);
        for &b in &bits {
            if let Some(g) = self.sync.push(b) {
                self.group(g);
            }
        }
        self.demod.bits = bits;
        self.demod.bits.clear();
        if self.sync.synced != was_locked && self.h.pi.is_some() {
            self.generation += 1;
        }
        self.quiet += mpx.len() as u64;
        self.samples += mpx.len() as u64;
        if self.h.pi.is_some() && self.quiet as f64 > FORGET_AFTER_S * self.fs {
            self.forget_station();
        }
    }

    /// Bumps whenever the station's data (not its error rate) changes.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Changes since the last call, for the decode log.
    pub fn take_changes(&mut self) -> Vec<RdsChange> {
        std::mem::take(&mut self.changes)
    }

    /// The station, once its PI is confirmed.
    pub fn station(&self) -> Option<RdsStation> {
        let pi = self.h.pi?;
        Some(RdsStation {
            pi: format!("{pi:04X}"),
            pi_call: pi_call(pi),
            ps: self.h.ps_out.clone(),
            ps_dynamic: self.h.ps_dynamic,
            pty: self.h.pty,
            pty_name: self.h.pty.map(pty_name),
            tp: self.h.tp,
            ta: self.h.ta,
            rt: self.h.rt_out.clone(),
            title: self.h.title.clone(),
            artist: self.h.artist.clone(),
            clock_utc: self.h.clock.as_ref().map(|c| c.0.clone()),
            clock_offset_min: self.h.clock.as_ref().map(|c| c.1),
            locked: self.sync.synced,
            pilot: self.demod.pilot.locked,
            bler: self.sync.bler(),
            updated_ms: 0,
        })
    }

    /// A log line for a change, from the station as it is now.
    pub fn event(&self, change: &RdsChange) -> Option<DecodeEvent> {
        let st = self.station()?;
        let mut fields = BTreeMap::new();
        fields.insert("pi".into(), st.pi.clone());
        if let Some(c) = &st.pi_call {
            fields.insert("call (from PI)".into(), c.clone());
        }
        if let Some(p) = &st.ps {
            fields.insert("ps".into(), p.clone());
        }
        if let Some(n) = st.pty_name {
            fields.insert("pty".into(), n.into());
        }
        if let Some(r) = &st.rt {
            fields.insert("radiotext".into(), r.clone());
        }
        if let Some(t) = &st.title {
            fields.insert("title".into(), t.clone());
        }
        if let Some(a) = &st.artist {
            fields.insert("artist".into(), a.clone());
        }
        // A scrolling PS is a word, not a name; the PI stands in for it.
        let name = st.ps.clone().filter(|_| !st.ps_dynamic).unwrap_or_else(|| format!("PI {}", st.pi));
        let (kind, summary) = match change {
            RdsChange::Ps => (
                "Program service",
                match st.pty_name {
                    Some(p) if st.pty != Some(0) => format!("{name} · {p}"),
                    _ => name,
                },
            ),
            RdsChange::Rt => ("RadioText", format!("{name} · {}", st.rt.clone()?)),
            RdsChange::RtPlus => (
                "RT+",
                match (&st.title, &st.artist) {
                    (Some(t), Some(a)) => format!("{name} · {t} — {a}"),
                    (Some(t), None) => format!("{name} · {t}"),
                    (None, Some(a)) => format!("{name} · {a}"),
                    (None, None) => return None,
                },
            ),
        };
        Some(DecodeEvent { protocol: "RDS".into(), kind: kind.into(), summary, valid: true, fields })
    }

    fn bump(&mut self) {
        self.generation += 1;
    }

    fn group(&mut self, g: [Option<Block>; 4]) {
        // PI, from block A or (in a version-B group) block C'; only exact
        // blocks count towards confirming it.
        let exact_pi = g[0]
            .filter(|b| b.ok == BlockOk::Exact)
            .map(|b| b.data)
            .or(g[2].filter(|b| b.c_prime && b.ok == BlockOk::Exact).map(|b| b.data));
        if let Some(pi) = exact_pi {
            if self.h.pi_cand == Some(pi) {
                self.h.pi_hits = self.h.pi_hits.saturating_add(1);
            } else {
                self.h.pi_cand = Some(pi);
                self.h.pi_hits = 1;
            }
            if self.h.pi_hits >= 2 && self.h.pi != Some(pi) {
                if self.h.pi.is_some() {
                    // Another station has taken the frequency.
                    self.forget_station();
                }
                self.h.pi = Some(pi);
                self.bump();
            }
        }
        let Some(pi) = self.h.pi else { return };
        // The group is this station's if it carries the PI, or if its PI
        // was unreadable but the last readable one in this unbroken run of
        // sync was the station's. A readable PI that differs never counts.
        let read_pi = g[0].map(|b| b.data).or(g[2].filter(|b| b.c_prime).map(|b| b.data));
        let ours = match read_pi {
            Some(p) if p == pi => {
                self.h.pi_run = Some(self.sync.run);
                true
            }
            Some(_) => {
                self.h.pi_run = None;
                false
            }
            None => self.h.pi_run == Some(self.sync.run),
        };
        if !ours {
            return;
        }
        let Some(b) = g[1] else { return };
        self.quiet = 0;
        // Flags and the program type are taken from exact blocks B only:
        // a corrected one is wrong too often to move them.
        let b_exact = b.ok == BlockOk::Exact;
        let b = b.data;
        let gtype = (b >> 12) as u8;
        let version_b = b & 0x0800 != 0;
        let tp = b & 0x0400 != 0;
        let pty = ((b >> 5) & 31) as u8;
        if b_exact {
            if self.h.pty_cand == Some(pty) && self.h.pty != Some(pty) {
                self.h.pty = Some(pty);
                self.bump();
            }
            self.h.pty_cand = Some(pty);
            if tp != self.h.tp && self.h.tp_cand == Some(tp) {
                self.h.tp = tp;
                self.bump();
            }
            self.h.tp_cand = Some(tp);
        }
        let c = g[2].filter(|b| !b.c_prime).map(|b| b.data);
        let d = g[3].map(|b| b.data);
        let c_exact = g[2].is_some_and(|b| b.ok == BlockOk::Exact && !b.c_prime);
        let d_exact = g[3].is_some_and(|b| b.ok == BlockOk::Exact);
        match (gtype, version_b) {
            (0, _) => {
                let ta = b & 0x10 != 0;
                if b_exact {
                    if ta != self.h.ta && self.h.ta_cand == Some(ta) {
                        self.h.ta = ta;
                        self.bump();
                    }
                    self.h.ta_cand = Some(ta);
                }
                if let Some(d) = d {
                    self.h.ps.put((b & 3) as usize, d.to_be_bytes(), b_exact && d_exact);
                    self.publish_ps();
                }
            }
            (2, vb) => {
                let ab = b & 0x10 != 0;
                if self.h.rt_ab.is_some_and(|x| x != ab) || self.h.rt_b.is_some_and(|x| x != vb) {
                    // The station says to clear its text, or this block B
                    // was misread: only exact blocks, two in a row, decide.
                    if !b_exact {
                        return;
                    }
                    if self.h.rt_flip != Some((ab, vb)) {
                        self.h.rt_flip = Some((ab, vb));
                        return;
                    }
                    self.h.rt.clear();
                    self.clear_tags();
                    if self.h.rt_out.take().is_some() {
                        self.h.rt_raw = None;
                        self.bump();
                    }
                }
                self.h.rt_flip = None;
                self.h.rt_ab = Some(ab);
                self.h.rt_b = Some(vb);
                let seg = (b & 0xF) as usize;
                let mut changed = false;
                if vb {
                    if let Some(d) = d {
                        changed |= self.h.rt.put(seg, d.to_be_bytes(), b_exact && d_exact);
                    }
                } else {
                    if let Some(c) = c {
                        changed |= self.h.rt.put(2 * seg, c.to_be_bytes(), b_exact && c_exact);
                    }
                    if let Some(d) = d {
                        changed |= self.h.rt.put(2 * seg + 1, d.to_be_bytes(), b_exact && d_exact);
                    }
                }
                if changed {
                    // A new text is being sent; tags heard before now were
                    // for the old one.
                    self.clear_tags();
                }
                self.publish_rt();
            }
            (3, false) => {
                if let Some(d) = d
                    && d == AID_RTPLUS
                {
                    let code = (b & 0x1F) as u8;
                    let grp = (code >> 1, code & 1 == 1);
                    self.h.rtplus_group = Some(grp);
                }
            }
            (4, false) => {
                if c_exact && d_exact {
                    self.clock_group(b, c.unwrap_or(0), d.unwrap_or(0));
                }
            }
            _ => {}
        }
        if self.h.rtplus_group == Some((gtype, version_b))
            && let (Some(c), Some(d)) = (c, d)
        {
            self.rtplus(b, c, d);
        }
    }

    fn publish_ps(&mut self) {
        if let Some(bytes) = self.h.ps.complete() {
            let s = to_string(&bytes);
            if !s.is_empty() && self.h.ps_out.as_deref() != Some(&s) {
                let now = self.samples;
                let minute = (60.0 * self.fs) as u64;
                self.h.ps_seen.retain(|(t, _)| now.saturating_sub(*t) < minute);
                self.h.ps_seen.push((now, s.clone()));
                let mut names: Vec<&str> = self.h.ps_seen.iter().map(|(_, n)| n.as_str()).collect();
                names.sort_unstable();
                names.dedup();
                // Three names in a minute is a scrolling PS, not a station
                // renaming itself: it stays in the status but out of the
                // log, which would otherwise take a line every few seconds.
                if names.len() >= 3 {
                    self.h.ps_dynamic = true;
                }
                self.h.ps_out = Some(s);
                self.bump();
                if !self.h.ps_dynamic {
                    self.changes.push(RdsChange::Ps);
                }
            }
        }
    }

    fn publish_rt(&mut self) {
        if let Some(bytes) = self.h.rt.complete() {
            let s = to_string(&bytes);
            if !s.is_empty() && self.h.rt_out.as_deref() != Some(&s) {
                self.h.rt_out = Some(s);
                self.h.rt_raw = Some(bytes);
                self.bump();
                self.changes.push(RdsChange::Rt);
                self.apply_tags();
            } else if !s.is_empty() {
                self.h.rt_raw = Some(bytes);
            }
        } else if !self.h.rt.any() && self.h.rt_out.is_some() {
            self.h.rt_out = None;
            self.h.rt_raw = None;
            self.bump();
        }
    }

    fn clear_tags(&mut self) {
        self.h.tags.clear();
        self.h.rtplus_cand = None;
        self.h.running = false;
        if self.h.title.take().is_some() | self.h.artist.take().is_some() {
            self.bump();
        }
    }

    fn rtplus(&mut self, b: u16, c: u16, d: u16) {
        let toggle = b & 0x10 != 0;
        let key = (b & 0x1F, c, d);
        if self.h.rtplus_cand != Some(key) {
            self.h.rtplus_cand = Some(key);
            return;
        }
        // Heard twice alike, so the toggle is the station's: a new item.
        if self.h.rtplus_toggle.is_some_and(|t| t != toggle) {
            self.clear_tags();
            self.h.rtplus_cand = Some(key);
        }
        self.h.rtplus_toggle = Some(toggle);
        let running = b & 0x08 != 0;
        let t1 = (((b & 0x07) << 3) | (c >> 13)) as u8;
        let s1 = ((c >> 7) & 0x3F) as usize;
        let l1 = ((c >> 1) & 0x3F) as usize;
        let t2 = (((c & 1) << 5) | (d >> 11)) as u8;
        let s2 = ((d >> 5) & 0x3F) as usize;
        let l2 = (d & 0x1F) as usize;
        self.h.running = running;
        self.h.tags = [(t1, s1, l1), (t2, s2, l2)].into_iter().filter(|t| t.0 != 0).collect();
        self.apply_tags();
    }

    fn apply_tags(&mut self) {
        let pick = |want: u8| -> Option<String> {
            let raw = self.h.rt_raw.as_ref()?;
            if !self.h.running {
                return None;
            }
            let &(_, start, len) = self.h.tags.iter().find(|t| t.0 == want)?;
            let end = start + len + 1;
            if end > raw.len() {
                return None;
            }
            let s = to_string(&raw[start..end]);
            (!s.is_empty()).then_some(s)
        };
        // 1 is ITEM.TITLE, 4 ITEM.ARTIST.
        let title = pick(1);
        let artist = pick(4);
        if title != self.h.title || artist != self.h.artist {
            self.h.title = title;
            self.h.artist = artist;
            self.bump();
            if self.h.title.is_some() || self.h.artist.is_some() {
                self.changes.push(RdsChange::RtPlus);
            }
        }
    }

    fn clock_group(&mut self, b: u16, c: u16, d: u16) {
        let mjd = (((b & 3) as u32) << 15) | (c >> 1) as u32;
        let hour = (((c & 1) << 4) | (d >> 12)) as u32;
        let minute = ((d >> 6) & 0x3F) as u32;
        let off = (d & 0x1F) as i32 * 30;
        let off = if d & 0x20 != 0 { -off } else { off };
        // A clock that cannot be a time is not reported.
        if hour > 23 || minute > 59 || off.abs() > 14 * 60 || !(58_849..=88_068).contains(&mjd) {
            return;
        }
        let (y, m, day) = mjd_to_date(mjd);
        let s = format!("{y:04}-{m:02}-{day:02}T{hour:02}:{minute:02}Z");
        if self.h.clock.as_ref().map(|c| (&c.0, c.1)) != Some((&s, off)) {
            self.h.clock = Some((s, off));
            self.bump();
        }
    }
}

/// Modified Julian Date to (year, month, day), by the formula in the RDS
/// standard's Annex G.
fn mjd_to_date(mjd: u32) -> (i32, u32, u32) {
    let mjd = mjd as f64;
    let yp = ((mjd - 15_078.2) / 365.25).floor();
    let mp = ((mjd - 14_956.1 - (yp * 365.25).floor()) / 30.6001).floor();
    let day = mjd - 14_956.0 - (yp * 365.25).floor() - (mp * 30.6001).floor();
    let k = if mp == 14.0 || mp == 15.0 { 1.0 } else { 0.0 };
    ((yp + k) as i32 + 1900, (mp - 1.0 - k * 12.0) as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkwords_match_the_offsets() {
        // A block of all zeros carries just its offset as the checkword.
        for o in [OFFSET_A, OFFSET_B, OFFSET_C, OFFSET_CP, OFFSET_D] {
            assert_eq!(syndrome(o as u32, o), 0);
        }
        assert_eq!(checkword(0), 0);
    }

    #[test]
    fn every_short_burst_is_corrected() {
        let table = burst_table();
        let word = (0x1234u32 << 10) | (checkword(0x1234) ^ OFFSET_B) as u32;
        for len in 1..=5u32 {
            for shift in 0..=(26 - len) {
                let e = ((1u32 << len) - 1) << shift;
                let bad = word ^ e;
                let fix = table[syndrome(bad, OFFSET_B) as usize];
                assert_eq!(bad ^ fix, word, "burst of {len} at {shift}");
            }
        }
        // No two bursts were found to share a syndrome.
        // 26 + 25 + 2·24 + 4·23 + 8·22 bursts, none sharing a syndrome.
        assert_eq!(table.iter().filter(|&&e| e != 0).count(), 367);
    }

    #[test]
    fn pi_to_call_letters() {
        assert_eq!(pi_call(4096).as_deref(), Some("KAAA"));
        assert_eq!(pi_call(21672).as_deref(), Some("WAAA"));
        // K + 10·676 + 16·26 + 4.
        assert_eq!(pi_call(11_276).as_deref(), Some("KKQE"));
        assert_eq!(pi_call(0xC000), None);
    }

    #[test]
    fn mjd_dates() {
        // 2026-11-03 is MJD 61347.
        assert_eq!(mjd_to_date(61_347), (2026, 11, 3));
        assert_eq!(mjd_to_date(51_544), (2000, 1, 1));
    }

    // ---------- a transmitter, for the end-to-end tests ----------

    fn block(data: u16, offset: u16) -> u32 {
        ((data as u32) << 10) | (checkword(data) ^ offset) as u32
    }

    fn group_bits(blocks: [u16; 4], version_b: bool, errors: &[(usize, u32)], out: &mut Vec<bool>) {
        let offs = [OFFSET_A, OFFSET_B, if version_b { OFFSET_CP } else { OFFSET_C }, OFFSET_D];
        for (k, (&data, &o)) in blocks.iter().zip(offs.iter()).enumerate() {
            let mut w = block(data, o);
            for &(blk, e) in errors {
                if blk == k {
                    w ^= e;
                }
            }
            for i in (0..26).rev() {
                out.push(w & (1 << i) != 0);
            }
        }
    }

    const PI: u16 = 0x54A8;
    const PTY: u16 = 9;

    fn b_word(gtype: u16, vb: bool, low5: u16) -> u16 {
        (gtype << 12) | ((vb as u16) << 11) | (1 << 10) | (PTY << 5) | (low5 & 0x1F)
    }

    /// Groups: PS, a 2A RadioText ended by a carriage return, the RT+
    /// announcement and tags (in group 11A), and the clock.
    fn program(ps: &str, rt: &str, title: (usize, usize), artist: (usize, usize)) -> Vec<([u16; 4], bool)> {
        let ps = ps.as_bytes();
        let mut rtb = rt.as_bytes().to_vec();
        rtb.push(0x0D);
        while rtb.len() % 4 != 0 {
            rtb.push(b' ');
        }
        let mut gs = Vec::new();
        for seg in 0..4u16 {
            let d = u16::from_be_bytes([ps[2 * seg as usize], ps[2 * seg as usize + 1]]);
            gs.push(([PI, b_word(0, false, seg), 0xE0CD, d], false));
        }
        for seg in 0..(rtb.len() / 4) {
            let c = u16::from_be_bytes([rtb[4 * seg], rtb[4 * seg + 1]]);
            let d = u16::from_be_bytes([rtb[4 * seg + 2], rtb[4 * seg + 3]]);
            gs.push(([PI, b_word(2, false, seg as u16), c, d], false));
            if seg % 3 == 0 {
                gs.push(([PI, b_word(0, false, 0), 0xE0CD, u16::from_be_bytes([ps[0], ps[1]])], false));
            }
        }
        gs.push(([PI, b_word(3, false, 11 << 1), 0, AID_RTPLUS], false));
        // Title (type 1) and artist (type 4), item running.
        let (ts, tl) = title;
        let (as_, al) = artist;
        let c = (1u16 & 7) << 13 | ((ts as u16) << 7) | ((tl as u16 - 1) << 1);
        let d = (4u16 << 11) | ((as_ as u16) << 5) | (al as u16 - 1);
        let rtp = [PI, b_word(11, false, 0x08), c, d];
        gs.push((rtp, false));
        // 2026-11-03 19:32 UTC, offset -5 h.
        let mjd: u32 = 61_347;
        let b4 = b_word(4, false, (mjd >> 15) as u16);
        let c4 = (((mjd & 0x7FFF) << 1) as u16) | 1; // hour 19 = 0b10011
        let d4 = (3u16 << 12) | (32 << 6) | 0x20 | 10;
        gs.push(([PI, b4, c4, d4], false));
        gs.push((rtp, false));
        // A version-B group, which carries the PI again in block 3.
        gs.push(([PI, b_word(15, true, 0), PI, 0], true));
        gs
    }

    struct Rng(u64);
    impl Rng {
        fn uniform(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
        fn gauss(&mut self) -> f64 {
            let u = self.uniform().max(1e-12);
            let v = self.uniform();
            (-2.0 * u.ln()).sqrt() * (TAU * v).cos()
        }
    }

    struct Tx {
        fs: f64,
        pilot: bool,
        rds_amp: f64,
        noise: f64,
        carrier_err_hz: f64,
    }

    /// The MPX signal for these data bits, in units of the full deviation:
    /// program audio, the pilot, the RDS subcarrier and white noise.
    fn mpx(tx: &Tx, data: &[bool], seed: u64) -> Vec<f32> {
        // Differential coding, then biphase chips.
        let mut t = false;
        let mut chips = Vec::with_capacity(data.len() * 2);
        for &d in data {
            t ^= d;
            let s = if t { 1.0 } else { -1.0 };
            chips.push(s);
            chips.push(-s);
        }
        let chip_rate = 2.0 * BIT_RATE * (1.0 + tx.carrier_err_hz / CARRIER_HZ);
        let n = (chips.len() as f64 / chip_rate * tx.fs) as usize;
        let mut shape = butter4(2_400.0, tx.fs);
        let mut rng = Rng(seed);
        let mut out = Vec::with_capacity(n);
        let ph0 = 0.7;
        for k in 0..n {
            let ts = k as f64 / tx.fs;
            let chip = chips[((ts * chip_rate) as usize).min(chips.len() - 1)];
            let mut v = chip;
            for f in &mut shape {
                v = f.process(v);
            }
            let audio = 0.35 * (TAU * 440.0 * ts).sin() + 0.2 * (TAU * 3_100.0 * ts).sin() + 0.1 * (TAU * 38_000.0 * ts).sin() * (TAU * 900.0 * ts).sin();
            let pilot_ph = TAU * (PILOT_HZ + tx.carrier_err_hz / 3.0) * ts + ph0;
            let pilot = if tx.pilot { 0.09 * pilot_ph.cos() } else { 0.0 };
            let carrier = (3.0 * pilot_ph + 0.4).cos();
            out.push((audio + pilot + tx.rds_amp * v * carrier + tx.noise * rng.gauss()) as f32);
        }
        out
    }

    fn air(cycles: usize, errors_every: usize) -> Vec<bool> {
        let gs = program("KXYZ FM ", "Blue Horizon by The Examples", (0, 12), (16, 12));
        let mut bits = Vec::new();
        let mut rng = Rng(99);
        let mut k = 0;
        for _ in 0..cycles {
            for (g, vb) in &gs {
                k += 1;
                let mut errs = Vec::new();
                if errors_every > 0 && k % errors_every == 0 {
                    // A burst of up to 5 bits in one block.
                    let blk = (rng.uniform() * 4.0) as usize;
                    let len = 1 + (rng.uniform() * 5.0) as u32;
                    let at = (rng.uniform() * (27 - len) as f64) as u32;
                    errs.push((blk, (((1u32 << len) - 1) | 1) << at));
                }
                group_bits(*g, *vb, &errs, &mut bits);
            }
        }
        bits
    }

    fn run(tx: &Tx, bits: &[bool]) -> (RdsDecoder, Vec<DecodeEvent>) {
        let mut dec = RdsDecoder::new(tx.fs);
        let signal = mpx(tx, bits, 7);
        let mut events = Vec::new();
        for chunk in signal.chunks(4096) {
            dec.process(chunk);
            for c in dec.take_changes() {
                events.extend(dec.event(&c));
            }
        }
        (dec, events)
    }

    #[test]
    fn decodes_ps_radiotext_tags_and_clock_with_a_pilot() {
        let tx = Tx { fs: 256_000.0, pilot: true, rds_amp: 0.04, noise: 0.05, carrier_err_hz: 0.0 };
        let (dec, events) = run(&tx, &air(5, 7));
        let st = dec.station().expect("a station");
        assert!(st.pilot, "carrier taken from the pilot");
        assert_eq!(st.pi, "54A8");
        assert_eq!(st.ps.as_deref(), Some("KXYZ FM"));
        assert_eq!(st.rt.as_deref(), Some("Blue Horizon by The Examples"));
        assert_eq!(st.title.as_deref(), Some("Blue Horizon"));
        assert_eq!(st.artist.as_deref(), Some("The Examples"));
        assert_eq!(st.pty, Some(9));
        assert_eq!(st.pty_name, Some("Top 40"));
        assert!(st.tp);
        assert_eq!(st.clock_utc.as_deref(), Some("2026-11-03T19:32Z"));
        assert_eq!(st.clock_offset_min, Some(-300));
        // One line each, not one a group.
        let kinds: Vec<_> = events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds.iter().filter(|k| **k == "Program service").count(), 1, "{kinds:?}");
        assert_eq!(kinds.iter().filter(|k| **k == "RadioText").count(), 1, "{kinds:?}");
        assert_eq!(kinds.iter().filter(|k| **k == "RT+").count(), 1, "{kinds:?}");
        assert!(events.iter().all(|e| e.protocol == "RDS" && e.valid));
    }

    #[test]
    fn decodes_a_mono_station_without_a_pilot() {
        let tx = Tx { fs: 250_000.0, pilot: false, rds_amp: 0.04, noise: 0.05, carrier_err_hz: 4.0 };
        let (dec, _) = run(&tx, &air(5, 0));
        let st = dec.station().expect("a station");
        assert!(!st.pilot);
        assert_eq!(st.ps.as_deref(), Some("KXYZ FM"));
        assert_eq!(st.rt.as_deref(), Some("Blue Horizon by The Examples"));
    }

    #[test]
    fn a_signal_too_weak_yields_nothing() {
        let tx = Tx { fs: 256_000.0, pilot: true, rds_amp: 0.002, noise: 0.4, carrier_err_hz: 0.0 };
        let (dec, events) = run(&tx, &air(4, 0));
        assert!(dec.station().is_none_or(|s| s.ps.is_none() && s.rt.is_none()));
        assert!(events.is_empty());
    }

    #[test]
    fn noise_alone_yields_nothing() {
        let mut dec = RdsDecoder::new(256_000.0);
        let mut rng = Rng(3);
        let noise: Vec<f32> = (0..256_000 * 6).map(|_| rng.gauss() as f32 * 0.3).collect();
        for chunk in noise.chunks(4096) {
            dec.process(chunk);
        }
        assert!(dec.station().is_none());
        assert!(dec.take_changes().is_empty());
    }

    #[test]
    fn a_scrolling_ps_is_flagged_and_kept_out_of_the_log() {
        let tx = Tx { fs: 256_000.0, pilot: true, rds_amp: 0.04, noise: 0.03, carrier_err_hz: 0.0 };
        let mut bits = Vec::new();
        for word in ["NOW     ", "PLAYING ", "BLUE    ", "HORIZON "] {
            let gs = program(word, "Blue Horizon by The Examples", (0, 12), (16, 12));
            // Each word held for a few seconds, as stations do.
            for _ in 0..3 {
                for (g, vb) in gs.iter().filter(|(g, _)| g[1] >> 12 == 0) {
                    group_bits(*g, *vb, &[], &mut bits);
                }
                for (g, vb) in gs.iter().filter(|(g, _)| g[1] >> 12 != 0).take(6) {
                    group_bits(*g, *vb, &[], &mut bits);
                }
            }
        }
        let (dec, events) = run(&tx, &bits);
        let st = dec.station().expect("a station");
        assert!(st.ps_dynamic);
        assert_eq!(st.ps.as_deref(), Some("HORIZON"));
        let ps_lines = events.iter().filter(|e| e.kind == "Program service").count();
        assert!(ps_lines <= 2, "{ps_lines} PS lines");
    }

    /// Unreadable block share once synced, and seconds until both PS and
    /// RadioText are out, at a given Eb/N0.
    fn measure(ebn0_db: f64, pilot: bool) -> (f64, f64) {
        let fs = 256_000.0;
        let a: f64 = 0.04;
        // Eb/N0 = A² fs / (4 · 1187.5 · σ²) for white noise of σ per sample.
        let sigma = (a * a * fs / (4.0 * BIT_RATE * 10f64.powf(ebn0_db / 10.0))).sqrt();
        let tx = Tx { fs, pilot, rds_amp: a, noise: sigma, carrier_err_hz: if pilot { 0.0 } else { 3.0 } };
        let mut dec = RdsDecoder::new(fs);
        let signal = mpx(&tx, &air(12, 0), 11);
        let (mut bad, mut n) = (0.0, 0usize);
        let mut first = None;
        for (k, chunk) in signal.chunks(4096).enumerate() {
            dec.process(chunk);
            if k > 60 && dec.sync.synced {
                bad += dec.sync.bler() as f64;
                n += 1;
            }
            if first.is_none() && dec.station().is_some_and(|s| s.ps.is_some() && s.rt.is_some()) {
                first = Some((k + 1) as f64 * 4096.0 / fs);
            }
        }
        (if n > 0 { bad / n as f64 } else { 1.0 }, first.unwrap_or(f64::NAN))
    }

    /// How the decoder fares as the signal weakens.
    ///
    /// ```text
    /// cargo test -p scannerd-engine --release bler_sweep -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn bler_sweep() {
        for ebn0_db in [1.0, 2.0, 3.0, 4.0, 6.0] {
            let (bler, t) = measure(ebn0_db, true);
            let (bler2, t2) = measure(ebn0_db, false);
            println!("SWEEP Eb/N0 {ebn0_db:4.1} dB · pilot: {:.0}% unreadable, PS+RT at {t:.1} s · no pilot: {:.0}%, at {t2:.1} s", bler * 100.0, bler2 * 100.0);
        }
    }

    #[test]
    fn one_misread_flag_does_not_wipe_the_radiotext() {
        let mut dec = RdsDecoder::new(256_000.0);
        let blk = |data: u16| Some(Block { data, ok: BlockOk::Exact, c_prime: false });
        let gs = program("KXYZ FM ", "Blue Horizon by The Examples", (0, 12), (16, 12));
        for _ in 0..3 {
            for (g, _) in gs.iter().filter(|(g, _)| g[1] >> 12 == 2) {
                dec.group([blk(g[0]), blk(g[1]), blk(g[2]), blk(g[3])]);
            }
        }
        assert_eq!(dec.station().unwrap().rt.as_deref(), Some("Blue Horizon by The Examples"));
        // A block B "corrected" to the other A/B flag, once.
        let (g, _) = gs.iter().find(|(g, _)| g[1] >> 12 == 2).unwrap();
        dec.group([blk(g[0]), blk(g[1] ^ 0x10), blk(g[2]), blk(g[3])]);
        dec.group([blk(g[0]), blk(g[1]), blk(g[2]), blk(g[3])]);
        assert_eq!(dec.station().unwrap().rt.as_deref(), Some("Blue Horizon by The Examples"));
        // The station really flipping it (two groups in a row) clears it.
        dec.group([blk(g[0]), blk(g[1] ^ 0x10), blk(g[2]), blk(g[3])]);
        dec.group([blk(g[0]), blk(g[1] ^ 0x10), blk(g[2]), blk(g[3])]);
        assert_eq!(dec.station().unwrap().rt, None);
    }

    #[test]
    fn a_text_counts_only_once_heard_twice() {
        let mut t = Text::new(4);
        for k in 0..4 {
            t.put(k, *b"AB", true);
        }
        assert!(t.complete().is_none());
        for k in 0..4 {
            t.put(k, *b"AB", true);
        }
        assert_eq!(t.complete().as_deref(), Some(&b"ABABABAB"[..]));
        // One pair changes: nothing is published until the rest is heard
        // again, so a half-old, half-new name never shows.
        t.put(1, *b"XY", true);
        t.put(1, *b"XY", true);
        assert!(t.complete().is_none());
        for k in [0, 2, 3] {
            t.put(k, *b"AB", true);
        }
        assert_eq!(t.complete().as_deref(), Some(&b"ABXYABAB"[..]));
    }

    #[test]
    fn a_scrolling_name_is_never_spliced_from_two_frames() {
        let mut t = Text::new(2);
        for _ in 0..2 {
            t.put(0, *b"AB", true);
            t.put(1, *b"CD", true);
        }
        assert_eq!(t.complete().as_deref(), Some(&b"ABCD"[..]));
        // The station moves to "EFGH": EF is confirmed, GH heard once...
        t.put(0, *b"EF", true);
        t.put(0, *b"EF", true);
        t.put(1, *b"GH", true);
        // ...then to "IJCD", whose CD matches what is held. "EFCD" was
        // never sent, so it must not come out.
        t.put(1, *b"CD", true);
        assert_eq!(t.complete(), None);
    }
}

