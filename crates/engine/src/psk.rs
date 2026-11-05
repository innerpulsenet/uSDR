//! BPSK31 and BPSK63 for the whole sideband at once: every PSK signal in
//! the passband is found, given a channel of its own, and decoded side by
//! side, the way a "PSK browser" lists a band.
//!
//! The audio is cut to about 8 kHz once, and that one stream feeds both the
//! detector and every channel, so a channel costs only its own mixer and
//! filters. The detector looks at an averaged spectrum for the shape a PSK
//! signal leaves: traffic peaks on the carrier and spreads about one baud
//! either side, idle is two narrow lines half a baud either side, and a CW
//! carrier, concentrated in a bin or two, is neither. A new channel finds
//! its carrier in the last three seconds of audio and then starts on them,
//! so it has the opening words of the transmission that got it noticed.
//! Each channel mixes its signal to 0 Hz, filters it to about sixteen
//! samples a symbol, and recovers the symbol clock (Gardner), the carrier
//! frequency (from the squared signal while it searches, which the
//! reversals do not disturb, and from the decided phase steps once it
//! copies) and a confidence for each bit (against a carrier phase taken
//! from the symbols either side, and from the step from the last symbol;
//! a reversal is 0). The characters are the most likely reading of those
//! confidences as Varicode, not one hard bit at a time. A squelch on how
//! cleanly the phase steps land on 0 or 180 degrees keeps noise from
//! printing; it rides through fades, which on the air are deep and quick,
//! and holds each character back until clean steps after it vouch for it.
//!
//! Text comes out a line at a time per channel: at a CR or LF, at 80
//! characters, or after two seconds with nothing new.

use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use scannerd_dsp::{DecimFir, Nco};
use std::f32::consts::PI;
use std::sync::Arc;

/// A run of decoded text from one channel.
#[derive(Debug, Clone, PartialEq)]
pub struct PskText {
    pub channel: u32,
    /// Carrier frequency in the audio, Hz, as tracked when the text was cut.
    pub audio_hz: f32,
    /// Signal to noise in 2500 Hz, dB, from the averaged spectrum.
    pub snr_db: f32,
    pub text: String,
}

/// One tracked signal.
#[derive(Debug, Clone, PartialEq)]
pub struct PskChannel {
    pub id: u32,
    pub audio_hz: f32,
    /// Signal to noise in 2500 Hz, dB, from the averaged spectrum.
    pub snr_db: f32,
    /// Squelch open: the channel is copying a PSK signal right now.
    pub active: bool,
    /// Characters decoded since the channel started.
    pub chars: u64,
}

/// The shared stream's rate is the input rate divided by a whole number,
/// the largest that keeps it at or above this. 7 kHz leaves room for a
/// 3.1 kHz passband and the filter's transition above it.
const FRONT_MIN_RATE: f32 = 7_000.0;
/// The top of the audio the shared filter keeps clean of aliases.
const FRONT_PASS_HZ: f32 = 3_150.0;
/// Samples a symbol in each channel, near enough: the channel's rate is the
/// shared rate divided by a whole number.
const SPS_TARGET: f32 = 16.0;
/// The carrier loop's reach either side of where the detector last saw
/// the signal. PSK signals drift, but rarely by more than this in the half
/// second between looks.
const AFC_RANGE_HZ: f32 = 15.0;

/// Averaged spectrum: an exponential average of Hann-windowed frames with a
/// time constant of this many seconds, long enough to flatten the noise
/// for a weak signal, short enough to see a new one within a second or two.
const SPEC_TAU_S: f32 = 2.0;
/// Frames advance by a quarter of their length.
const SPEC_OVERLAP: usize = 4;
/// The detector runs once every this many frames.
const DETECT_EVERY: u32 = 4;
/// Frames averaged before the first detection: a younger average is too
/// rough for its noise floor to be trusted.
const MIN_FRAMES: u32 = 24;
/// The noise floor is measured over pieces of the band this wide, so a
/// sideband filter's slope does not read as signal.
const FLOOR_SEG_HZ: f32 = 400.0;
/// A window of plus and minus one baud must average this much power above
/// the noise, per bin, to be a candidate: about -19 dB in 2500 Hz for
/// BPSK31, well below what copies, and far above what noise averages to.
const DETECT_EXCESS: f32 = 0.4;
/// A candidate whose power sits more than this share within two bins of
/// its strongest is a carrier, not PSK.
const CW_SHARE: f32 = 0.7;
/// A candidate this much weaker than one within four baud is taken for the
/// stronger signal's sidelobes.
const SIDELOBE_RATIO: f32 = 300.0;

/// Squelch: the average of cos(2 x phase-step error) opens above this...
const SQ_OPEN: f32 = 0.35;
/// ...and closes below this. Noise averages zero with a spread of 0.1, so
/// a channel left on noise closes within a second of its signal leaving.
const SQ_CLOSE: f32 = 0.12;
/// The squelch average's step per symbol. Noise averages near zero with a
/// spread of about 0.1 at this length, so it seldom reaches the threshold.
const SQ_ALPHA: f32 = 1.0 / 48.0;
/// A plain mean of the same over the last few symbols. Unlike an
/// exponential average it forgets a signal entirely that many symbols
/// after it ends. Noise spreads it by about 0.25 around zero, so above
/// [`SQ_SHORT_GOOD`] it says plainly that a signal is there. It marks how
/// far text can be trusted (see `last_good`), and opening needs it too.
///
/// Neither average closes the squelch quickly, and the signal's power is
/// not used at all: on the air a station fades by 10 dB and more in
/// mid-word, and a weak one's short mean dips to noise for a few symbols
/// at a time, while the text through it is mostly right. A squelch that
/// closed on either lost the characters around the dip.
const SQ_SHORT: usize = 8;
const SQ_SHORT_GOOD: f32 = 0.7;
/// A character is vouched for by a short mean above [`SQ_SHORT_GOOD`]
/// taken wholly after it.
const GOOD_AFTER: u64 = SQ_SHORT as u64;
/// When the squelch shuts, a character not yet vouched for is kept if the
/// mean score over its own bits reached this and it ended while the signal
/// was still plainly there, and so is each before it that did.
const HELD_QUALITY: f32 = 0.6;
/// A character is held at least this many symbols before it is printed.
const COMMIT_DELAY: u64 = 16;
/// The carrier phase reference for a symbol is taken from this many
/// symbols either side of it: five in all, 160 ms, short against HF
/// fading at a 1 Hz Doppler spread, long enough to average the noise.
const REF_HALF: u64 = 2;
/// Symbols remembered for the reference and for scoring characters.
const SYM_RING: usize = 128;
/// The wide frequency loop's weight once the carrier has been found in
/// the heard audio. Left at full weight it wandered a weak signal off by
/// a hertz or two, which was most of what kept its squelch shut.
const WIDE_FOUND: f32 = 0.25;
/// A new channel's loops run fast, and its squelch averages over half as
/// many symbols, until this many symbols after it first opens (or this
/// many times eight, if it never does). A new channel was started for a
/// signal the detector saw, so the faster squelch rarely meets noise alone,
/// and the replayed audio it starts on often holds only a second of idle
/// before the first words.
const ACQ_SYMBOLS: u64 = 32;

/// No character for this long and not seen by the detector for
/// [`RETIRE_UNSEEN_S`]: the channel is dropped.
const RETIRE_IDLE_S: f32 = 30.0;
const RETIRE_UNSEEN_S: f32 = 5.0;
/// No character for this long: dropped even if something is still there.
const RETIRE_HARD_S: f32 = 60.0;
/// A channel this idle may give its place to a stronger newcomer.
const EVICT_IDLE_S: f32 = 10.0;
/// Pending text is printed after this long without a new character.
const FLUSH_S: f32 = 2.0;
const LINE_MAX: usize = 80;
/// Audio kept so a new channel can start from before it was detected,
/// which is where a transmission's first words are.
const REPLAY_S: f32 = 3.0;

/// The PSK31 Varicode, ASCII 0 to 127. No code holds two zeros in a row;
/// "00" is the gap between characters.
const VARICODE: [&str; 128] = [
    "1010101011", "1011011011", "1011101101", "1101110111", "1011101011", "1101011111", "1011101111", "1011111101",
    "1011111111", "11101111", "11101", "1101101111", "1011011101", "11111", "1101110101", "1110101011",
    "1011110111", "1011110101", "1110101101", "1110101111", "1101011011", "1101101011", "1101101101", "1101010111",
    "1101111011", "1101111101", "1110110111", "1101010101", "1101011101", "1110111011", "1011111011", "1101111111",
    "1", "111111111", "101011111", "111110101", "111011011", "1011010101", "1010111011", "101111111",
    "11111011", "11110111", "101101111", "111011111", "1110101", "110101", "1010111", "110101111",
    "10110111", "10111101", "11101101", "11111111", "101110111", "101011011", "101101011", "110101101",
    "110101011", "110110111", "11110101", "110111101", "111101101", "1010101", "111010111", "1010101111",
    "1010111101", "1111101", "11101011", "10101101", "10110101", "1110111", "11011011", "11111101",
    "101010101", "1111111", "111111101", "101111101", "11010111", "10111011", "11011101", "10101011",
    "11010101", "111011101", "10101111", "1101111", "1101101", "101010111", "110110101", "101011101",
    "101110101", "101111011", "1010101101", "111110111", "111101111", "111111011", "1010111111", "101101101",
    "1011011111", "1011", "1011111", "101111", "101101", "11", "111101", "1011011",
    "101011", "1101", "111101011", "10111111", "11011", "111011", "1111", "111",
    "111111", "110111111", "10101", "10111", "101", "110111", "1111011", "1101011",
    "11011111", "1011101", "111010101", "1010110111", "110111011", "1010110101", "1011010111", "1110110101",
];

/// Bits of soft decisions the Varicode search looks back over before it
/// settles a character: about a second at 31 baud, several characters,
/// which is where the best reading of a damaged stretch stops changing.
const VARI_DELAY: usize = 32;
/// The longest token: a ten-bit code and its gap.
const VARI_MAX_TOKEN: usize = 12;

/// One character the search may read.
struct Code {
    /// The code's bits, first sent in the highest of `len`.
    bits: u16,
    len: u8,
    ch: u8,
}

/// A character the search has settled: its bits ran from symbol `start`
/// to `end`, its gap included.
#[derive(Clone, Copy)]
struct Settled {
    ch: u8,
    start: u64,
    end: u64,
}

/// Soft-decision Varicode: the most likely reading of the bit stream as
/// characters, not one bit at a time.
///
/// The stream is a run of tokens, each either a spare 0 (idle, or the gap
/// between characters being more than two bits) or a code followed by
/// "00". Every bit comes with a signed confidence, and a reading scores
/// the sum of the confidences it agrees with less those it contradicts. A
/// dynamic program keeps, for every bit position, the best reading that
/// ends a token there; once a stretch is far enough behind that every
/// good reading agrees on it, its characters are settled.
///
/// A hard decoder fails on one bad bit in a gap: two characters run into
/// one code that does not exist, and both are lost, or a bad bit in a
/// code splits it. Here the reading weighs that one weak bit against the
/// others, and a bit from a faded symbol is weak. Only printable ASCII,
/// CR and LF can be read: nobody sends the other control codes, and
/// allowing them only gives noise more to match.
struct SoftVaricode {
    codes: Vec<Code>,
    /// Confidences, positive for 1, of the bits since `base`.
    llr: Vec<f32>,
    /// `best[i]`: score of the best reading of `llr[committed..i]` that
    /// ends a token at `i`; `from[i]` where that token began and `tok[i]`
    /// its character, or -1 for a spare 0.
    best: Vec<f32>,
    from: Vec<u32>,
    tok: Vec<i16>,
    /// `run[i]`: score of the best reading that is, at `i`, inside a run
    /// of unbroken 1s longer than any code (a carrier with no reversals,
    /// which many programs send at the end of an over), and where that
    /// run began.
    run: Vec<f32>,
    run_from: Vec<u32>,
    /// The symbol `llr[0]` came from.
    base: u64,
    /// Everything before this position is settled.
    committed: usize,
    path: Vec<(u32, u32, i16)>,
}

impl SoftVaricode {
    fn new() -> Self {
        let codes = VARICODE
            .iter()
            .enumerate()
            .filter(|&(c, _)| (32..127).contains(&c) || c == 10 || c == 13)
            .map(|(c, code)| Code {
                bits: code.bytes().fold(0u16, |a, b| (a << 1) | u16::from(b == b'1')),
                len: code.len() as u8,
                ch: c as u8,
            })
            .collect();
        Self {
            codes,
            llr: Vec::with_capacity(1024),
            best: vec![0.0],
            from: vec![0],
            tok: vec![-1],
            run: vec![f32::NEG_INFINITY],
            run_from: vec![0],
            base: 0,
            committed: 0,
            path: Vec::with_capacity(64),
        }
    }

    /// Forget everything and start again at the next bit, as if after a
    /// gap.
    fn reset(&mut self) {
        self.llr.clear();
        self.best.clear();
        self.best.push(0.0);
        self.from.clear();
        self.from.push(0);
        self.tok.clear();
        self.tok.push(-1);
        self.run.clear();
        self.run.push(f32::NEG_INFINITY);
        self.run_from.clear();
        self.run_from.push(0);
        self.committed = 0;
    }

    /// The best reading ending a token at `n`, from `best` before it.
    fn extend(&mut self, n: usize) {
        let l = &self.llr;
        // A spare 0.
        let mut best = self.best[n - 1] - 0.5 * l[n - 1];
        let mut from = (n - 1) as u32;
        let mut tok = -1i16;
        for c in &self.codes {
            let len = c.len as usize;
            if n < self.committed + len + 2 {
                continue;
            }
            let start = n - len - 2;
            let mut sc = self.best[start] - 0.5 * (l[n - 1] + l[n - 2]);
            for (k, &v) in l[start..start + len].iter().enumerate() {
                if c.bits >> (len - 1 - k) & 1 == 1 {
                    sc += 0.5 * v;
                } else {
                    sc -= 0.5 * v;
                }
            }
            if sc > best {
                best = sc;
                from = start as u32;
                tok = c.ch as i16;
            }
        }
        // A run of 1s: one more, or a new one as long as the longest code
        // and one bit more. It ends at any bit, printing nothing; without
        // it a carrier's 1s had to be read as some code, and came out as
        // a row of "!" (nine 1s).
        const RUN: usize = VARI_MAX_TOKEN - 1;
        let (mut run, mut run_from) = (f32::NEG_INFINITY, 0u32);
        if n - 1 > self.committed && self.run_from[n - 1] as usize >= self.committed {
            run = self.run[n - 1] + 0.5 * l[n - 1];
            run_from = self.run_from[n - 1];
        }
        if n >= self.committed + RUN {
            let start = n - RUN;
            let sc = self.best[start] + 0.5 * l[start..n].iter().sum::<f32>();
            if sc > run {
                run = sc;
                run_from = start as u32;
            }
        }
        self.run[n] = run;
        self.run_from[n] = run_from;
        if run > best {
            best = run;
            from = run_from;
            tok = -1;
        }
        self.best[n] = best;
        self.from[n] = from;
        self.tok[n] = tok;
    }

    /// Trace the best reading back from `n` to the settled point, into
    /// `path` in sending order.
    fn trace(&mut self, n: usize) {
        self.path.clear();
        let mut i = n;
        while i > self.committed {
            let f = self.from[i] as usize;
            self.path.push((f as u32, i as u32, self.tok[i]));
            i = f;
        }
        self.path.reverse();
    }

    fn settle(&mut self, upto: usize, out: &mut Vec<Settled>) {
        let mut last = self.committed;
        for &(f, e, t) in &self.path {
            if e as usize > upto {
                break;
            }
            if t >= 0 {
                out.push(Settled { ch: t as u8, start: self.base + f as u64, end: self.base + e as u64 - 1 });
            }
            last = e as usize;
        }
        if last == self.committed {
            return;
        }
        self.committed = last;
        // Readings that began before the settled point no longer count.
        for n in last + 1..self.best.len() {
            self.extend(n);
        }
        if self.committed > 512 {
            let k = self.committed;
            self.llr.drain(..k);
            self.best.drain(..k);
            self.from.drain(..k);
            self.tok.drain(..k);
            self.run.drain(..k);
            self.run_from.drain(..k);
            self.from.iter_mut().for_each(|f| *f = f.saturating_sub(k as u32));
            self.run_from.iter_mut().for_each(|f| *f = f.saturating_sub(k as u32));
            self.base += k as u64;
            self.committed = 0;
        }
    }

    /// One more bit, confidence `l` (positive for 1), from symbol `symbol`.
    /// Characters that have settled go to `out`.
    fn push(&mut self, l: f32, symbol: u64, out: &mut Vec<Settled>) {
        if self.llr.is_empty() {
            self.base = symbol;
        }
        self.llr.push(l);
        self.best.push(f32::NEG_INFINITY);
        self.from.push(0);
        self.tok.push(-1);
        self.run.push(f32::NEG_INFINITY);
        self.run_from.push(0);
        let n = self.llr.len();
        self.extend(n);
        // Settled in batches: each settling recomputes the open stretch.
        if n - self.committed > VARI_DELAY + VARI_MAX_TOKEN + 16 {
            self.trace(n);
            self.settle(n - VARI_DELAY, out);
        }
    }

    /// Settle everything on the best reading so far, then start again.
    fn flush(&mut self, out: &mut Vec<Settled>) {
        let n = self.llr.len();
        if n > self.committed {
            self.trace(n);
            self.settle(n, out);
        }
        self.reset();
    }
}

/// Blackman-Harris windowed-sinc lowpass with unity gain at DC.
fn lowpass(cutoff_hz: f32, fs: f32, ntaps: usize) -> Vec<f32> {
    let fc = cutoff_hz / fs;
    let mid = (ntaps / 2) as f32;
    let mut taps: Vec<f32> = (0..ntaps)
        .map(|i| {
            let x = i as f32 - mid;
            let sinc = if x == 0.0 { 2.0 * fc } else { (2.0 * PI * fc * x).sin() / (PI * x) };
            let xw = 2.0 * PI * i as f32 / (ntaps - 1) as f32;
            let w = 0.358_75 - 0.488_29 * xw.cos() + 0.141_28 * (2.0 * xw).cos() - 0.011_68 * (3.0 * xw).cos();
            sinc * w
        })
        .collect();
    let sum: f32 = taps.iter().sum();
    taps.iter_mut().for_each(|t| *t /= sum);
    taps
}

/// A Hann pulse of `n` taps with unity gain at DC.
fn hann(n: usize) -> Vec<f32> {
    let mut h: Vec<f32> = (0..n).map(|i| 0.5 - 0.5 * (2.0 * PI * (i as f32 + 0.5) / n as f32).cos()).collect();
    let s: f32 = h.iter().sum();
    h.iter_mut().for_each(|v| *v /= s);
    h
}

/// Inner product in eight independent lanes, so the compiler can keep
/// them in vector registers.
#[inline]
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0.0f32; 8];
    let (ca, cb) = (a.chunks_exact(8), b.chunks_exact(8));
    let (ra, rb) = (ca.remainder(), cb.remainder());
    for (x, h) in ca.zip(cb) {
        for l in 0..8 {
            acc[l] += x[l] * h[l];
        }
    }
    let mut s = acc.iter().sum::<f32>();
    for (x, h) in ra.iter().zip(rb) {
        s += x * h;
    }
    s
}

/// What every channel shares: rates and filter designs.
struct Shape {
    fs1: f32,
    baud: f32,
    decim: usize,
    fs2: f32,
    sps: f32,
    fir_cutoff: f32,
    fir_taps: usize,
    /// Matched filter: a Hann pulse two symbols long, the shape PSK31 sends.
    mf: Vec<f32>,
    /// The wide frequency discriminator's filter: a Hann pulse one symbol
    /// long.
    wide: Vec<f32>,
}

impl Shape {
    fn new(fs1: f32, baud: f32) -> Self {
        let decim = ((fs1 / (SPS_TARGET * baud)).round() as usize).max(1);
        let fs2 = fs1 / decim as f32;
        let sps = fs2 / baud;
        // The band the channel filter keeps clean: the signal's main lobe
        // plus the carrier loop's reach. What would fold onto it starts at
        // the channel rate less that, and the cutoff sits halfway.
        let keep = 1.5 * baud + AFC_RANGE_HZ;
        let width = (fs2 - 2.0 * keep).max(0.2 * fs2);
        let fir_taps = (((8.0 * fs1 / width).ceil() as usize) | 1).clamp(31, 255);
        let l = (2.0 * sps).round() as usize;
        let wide = hann(l / 2);
        Self { fs1, baud, decim, fs2, sps, fir_cutoff: 0.5 * fs2, fir_taps, mf: hann(l), wide }
    }
}

/// One decoded character waiting out [`COMMIT_DELAY`].
#[derive(Clone, Copy, Default)]
struct Held {
    ch: u8,
    symbol: u64,
    /// The mean phase-step score over the character's own bits, its gap
    /// included: how cleanly it came in.
    quality: f32,
}

const HELD_MAX: usize = 32;
/// Symbol-timing history: enough for half a symbol back plus one, at any
/// samples-a-symbol this module chooses.
const RING: usize = 64;

struct Channel {
    id: u32,
    /// Where the detector last put the signal; the carrier loop stays
    /// within [`AFC_RANGE_HZ`] of it.
    home_hz: f32,
    /// The mixer's frequency. The carrier loop's correction is applied
    /// after the channel filter, at the channel rate, so its effect is not
    /// delayed by the filter.
    nco_hz: f32,
    fine_hz: f32,
    nco: Nco,
    fir: DecimFir,
    rot: Complex32,
    rot_step: Complex32,
    /// Matched-filter history, written twice so the window is contiguous.
    mf_hist: Vec<Complex32>,
    mf_pos: usize,
    ring: [Complex32; RING],
    ring_pos: usize,
    /// Channel samples until the next symbol strobe.
    t_next: f32,
    /// Carrier phase reference for coherent decisions, as a unit phasor,
    /// and the last symbol's sign against it.
    /// Recent symbols and their squelch scores, by symbol number.
    syms: [Complex32; SYM_RING],
    m_hist: [f32; SYM_RING],
    /// The two-sided carrier phase reference, kept for its sign.
    phase_ref: Complex32,
    prev_x: f32,
    prev_sym: Complex32,
    prev_y2: Complex32,
    prev_p: f32,
    /// Symbol-spaced coherence of the squared symbols: how steadily they
    /// turn from one symbol to the next, near 1 for a carrier however far
    /// off it is and near 0.2 for noise.
    coh_num: Complex32,
    coh_den: f32,
    /// The channel's quietest recent symbol power: falls at once, rises
    /// slowly, so it sits on the noise between transmissions.
    quiet: f32,
    fll_a: Complex32,
    fll_b: f32,
    /// Symbol power: average and fast average.
    p_avg: f32,
    p_fast: f32,
    q: f32,
    /// The last [`SQ_SHORT`] phase-step scores, and their mean.
    q_ring: [f32; SQ_SHORT],
    q_short: f32,
    open: bool,
    symbols: u64,
    /// The last symbol at which the short squelch average said plainly
    /// that a signal was there.
    last_good: u64,
    /// The symbol the squelch first opened at.
    opened_at: Option<u64>,
    vari: SoftVaricode,
    settled: Vec<Settled>,
    /// The symbol the squelch last opened at.
    open_since: u64,
    /// Nothing but spaces printed since the squelch opened or a line ended.
    fresh: bool,
    /// The carrier was found in the heard audio before the channel began,
    /// so the wide frequency loop has little to add but its noise.
    carrier_found: bool,
    held: [Held; HELD_MAX],
    n_held: usize,
    line: String,
    chars: u64,
    /// Shared-rate sample clock at the last character, and at the last
    /// time the detector saw the signal.
    last_char: u64,
    last_seen: u64,
    snr_db: f32,
}

impl Channel {
    fn new(id: u32, hz: f32, snr_db: f32, now: u64, sh: &Shape) -> Self {
        let mut nco = Nco::new();
        nco.set_freq(hz as f64, sh.fs1 as f64);
        let l = sh.mf.len();
        let zero = Complex32::new(0.0, 0.0);
        Self {
            id,
            home_hz: hz,
            nco_hz: hz,
            fine_hz: 0.0,
            nco,
            fir: DecimFir::new(sh.fir_cutoff, sh.fs1, sh.decim, sh.fir_taps),
            rot: Complex32::new(1.0, 0.0),
            rot_step: Complex32::new(1.0, 0.0),
            mf_hist: vec![zero; 2 * l],
            mf_pos: 0,
            ring: [zero; RING],
            ring_pos: 0,
            t_next: sh.sps,
            syms: [zero; SYM_RING],
            m_hist: [0.0; SYM_RING],
            phase_ref: Complex32::new(1.0, 0.0),
            prev_x: 0.0,
            prev_sym: zero,
            prev_y2: zero,
            prev_p: 0.0,
            coh_num: zero,
            coh_den: 0.0,
            quiet: 0.0,
            fll_a: zero,
            fll_b: 0.0,
            p_avg: 0.0,
            p_fast: 0.0,
            q: 0.0,
            q_ring: [0.0; SQ_SHORT],
            q_short: 0.0,
            open: false,
            symbols: 0,
            last_good: 0,
            opened_at: None,
            vari: SoftVaricode::new(),
            settled: Vec::with_capacity(16),
            open_since: 0,
            fresh: true,
            carrier_found: false,

            held: [Held::default(); HELD_MAX],
            n_held: 0,
            line: String::with_capacity(LINE_MAX + 8),
            chars: 0,
            last_char: now,
            last_seen: now,
            snr_db,
        }
    }

    fn hz(&self) -> f32 {
        self.nco_hz + self.fine_hz
    }

    fn set_fine(&mut self, hz: f32, sh: &Shape) {
        self.fine_hz = hz;
        let (s, c) = (-2.0 * PI * hz / sh.fs2).sin_cos();
        self.rot_step = Complex32::new(c, s);
    }

    /// Move the mixer to where the carrier loop has the signal, so the
    /// correction after the filter stays small.
    fn recenter(&mut self, hz: f32, sh: &Shape) {
        self.nco_hz = hz;
        self.nco.set_freq(hz as f64, sh.fs1 as f64);
        self.set_fine(0.0, sh);
    }

    fn process(&mut self, xc: &[Complex32], scratch: &mut Scratch, sh: &Shape, now: u64, out: &mut Vec<PskText>) {
        self.nco.mix(xc, &mut scratch.mixed);
        self.fir.process(&scratch.mixed, &mut scratch.dec);
        for &z in &scratch.dec {
            self.sample(z, sh, now, out);
        }
    }

    fn sample(&mut self, z: Complex32, sh: &Shape, now: u64, out: &mut Vec<PskText>) {
        let z = z * self.rot;
        self.rot *= self.rot_step;
        let l = sh.mf.len();
        self.mf_hist[self.mf_pos] = z;
        self.mf_hist[self.mf_pos + l] = z;
        self.mf_pos = (self.mf_pos + 1) % l;
        let win = &self.mf_hist[self.mf_pos..self.mf_pos + l];
        let mut y = Complex32::new(0.0, 0.0);
        for (x, &h) in win.iter().zip(&sh.mf) {
            y += x * h;
        }
        let mut w = Complex32::new(0.0, 0.0);
        for (x, &h) in win[l - sh.wide.len()..].iter().zip(&sh.wide) {
            w += x * h;
        }
        // The squared signal has no reversals: the phase steps through 180
        // degrees only where the amplitude passes through zero, and
        // squaring folds both sides onto one ray. Its rotation from sample
        // to sample is twice the carrier's frequency error. It is taken
        // from a filter twice as wide as the matched one: the matched
        // filter falls so steeply across the idle lines at half the baud
        // that, with the carrier off center, it weights one line over the
        // other enough to pull the estimate the wrong way. The wider one
        // still puts a neighbor 100 Hz away 44 dB down.
        let y2 = w * w;
        let p = w.norm_sqr();
        self.fll_a += y2 * self.prev_y2.conj();
        self.fll_b += p * self.prev_p;
        self.prev_y2 = y2;
        self.prev_p = p;
        self.ring_pos = (self.ring_pos + 1) % RING;
        self.ring[self.ring_pos] = y;
        self.t_next -= 1.0;
        if self.t_next <= 0.0 {
            self.symbol(-self.t_next, sh, now, out);
        }
    }

    /// The matched-filter output `back` samples before the newest one.
    fn interp(&self, back: f32) -> Complex32 {
        let i = back as usize;
        let f = back - i as f32;
        let a = self.ring[(self.ring_pos + RING - i) % RING];
        let b = self.ring[(self.ring_pos + RING - i - 1) % RING];
        a * (1.0 - f) + b * f
    }

    fn symbol(&mut self, frac: f32, sh: &Shape, now: u64, out: &mut Vec<PskText>) {
        let yk = self.interp(frac);
        let ym = self.interp(frac + 0.5 * sh.sps);
        let prev = self.prev_sym;
        self.prev_sym = yk;
        self.symbols += 1;
        let acquiring = match self.opened_at {
            Some(s) => self.symbols < s + ACQ_SYMBOLS,
            None => self.symbols < 8 * ACQ_SYMBOLS,
        };
        let pw = yk.norm_sqr();
        self.p_avg += (pw - self.p_avg) * 0.05;

        // Gardner: the midpoint between two opposite symbols is zero when
        // the strobes are centered, and leans toward the later symbol when
        // they come late. The product is the same at any carrier phase.
        let e = (ym.conj() * (prev - yk)).re / (self.p_avg + 1e-30);
        let kt = if acquiring { 0.04 } else { 0.012 };
        self.t_next += sh.sps * (1.0 + kt * e.clamp(-1.0, 1.0));

        // Differential decision: the phase step from the last symbol.
        let d = yk * prev.conj();
        let dn = d.norm_sqr();

        // Frequency. While copying, the phase step with the decided bit
        // taken out is the rotation over one symbol, from the matched
        // filter and so with the least noise; it reaches only a quarter of
        // the baud, which a locked channel never nears. Otherwise, Im of
        // the summed squared-signal steps over their magnitude is the sine
        // of twice the per-sample error, which reaches far wider. Both give
        // noise a bounded, zero-mean answer, so it only jitters the loop.
        //
        // The wide one is weighted by how coherent the squared symbols
        // are and by how far the channel stands above its own quiet, so
        // that with no signal (before a replayed transmission starts, or
        // after it ends) the loop holds still instead of wandering off or
        // drifting toward a strong neighbor's leakage, which is coherent
        // but faint.
        self.p_fast += (pw - self.p_fast) * 0.25;
        if self.symbols <= 4 || self.p_fast < self.quiet {
            self.quiet = self.p_fast;
        } else {
            self.quiet *= 1.002;
        }
        let lift = ((self.p_fast / (self.quiet + 1e-30) - 2.0) / 2.0).clamp(0.0, 1.0);
        let s2 = yk * yk;
        self.coh_num += (s2 * (prev * prev).conj() - self.coh_num) / 16.0;
        self.coh_den += (s2.norm() * prev.norm_sqr() - self.coh_den) / 16.0;
        let coh = self.coh_num.norm() / (self.coh_den + 1e-30);
        let (df, weight) = if self.open {
            let r = if d.re >= 0.0 { d } else { -d };
            (r.im / (dn.sqrt() + 1e-30) * sh.baud / (2.0 * PI), 1.0)
        } else {
            let trust = if self.carrier_found { WIDE_FOUND } else { 1.0 };
            (self.fll_a.im / (self.fll_b + 1e-30) * sh.fs2 / (4.0 * PI), trust * lift * ((coh - 0.35) / 0.35).clamp(0.0, 1.0))
        };
        self.fll_a = Complex32::new(0.0, 0.0);
        self.fll_b = 0.0;
        let kf = weight
            * match (self.open, acquiring) {
                (true, true) => 0.05,
                (true, false) => 0.02,
                (false, true) => 0.08,
                (false, false) => 0.03,
            };
        let target = (self.hz() + kf * df).clamp(self.home_hz - AFC_RANGE_HZ, self.home_hz + AFC_RANGE_HZ);
        self.set_fine(target - self.nco_hz, sh);
        // The rotator is renormalized each symbol; its rounding loss over
        // sixteen samples is far below anything that matters.
        self.rot *= 1.5 - 0.5 * self.rot.norm_sqr();

        let m = if dn > 0.0 { (d.re * d.re - d.im * d.im) / dn } else { 0.0 };

        self.q += (m - self.q) * if acquiring { 2.0 * SQ_ALPHA } else { SQ_ALPHA };
        let slot = (self.symbols % SQ_SHORT as u64) as usize;
        self.q_ring[slot] = m;
        self.q_short = self.q_ring.iter().sum::<f32>() / SQ_SHORT as f32;
        let k = self.symbols;
        self.syms[(k % SYM_RING as u64) as usize] = yk;
        self.m_hist[(k % SYM_RING as u64) as usize] = m;
        self.squelch(now, out);

        // The bit for the symbol REF_HALF back. Its carrier phase is taken
        // from the squared symbols either side of it as well as itself
        // (squaring strips the modulation), so the reference neither lags
        // a fading carrier's turning nor carries one symbol's noise whole.
        // The sign of the square root is kept by continuity, and a wrong
        // choice in a deep fade costs one bit, as a step from the last
        // symbol would there anyway.
        if k <= 2 * REF_HALF {
            return;
        }
        let j = k - REF_HALF;
        let at = |i: u64| self.syms[(i % SYM_RING as u64) as usize];
        let mut r2 = Complex32::new(0.0, 0.0);
        for i in j - REF_HALF..=j + REF_HALF {
            r2 += at(i) * at(i);
        }
        let mut rf = r2.sqrt();
        if (rf * self.phase_ref.conj()).re < 0.0 {
            rf = -rf;
        }
        self.phase_ref = rf;
        let rn = rf.norm() + 1e-30;
        let yj = at(j);
        let x = (yj * rf.conj()).re / rn;
        // The bit's confidence, positive for 1 (no reversal), from two
        // readings added: against the reference (the weaker of the two
        // symbols it compares, which is what that comparison can bear),
        // and the plain step from the last symbol, which recovers at once
        // from a reference that slipped. Under fading neither alone was
        // best everywhere; their sum was within a little of the better.
        // A faded symbol is small in both, so it counts for little.
        let coherent = if (x >= 0.0) == (self.prev_x >= 0.0) { 1.0 } else { -1.0 } * x.abs().min(self.prev_x.abs());
        let step = (yj * at(j - 1).conj()).re / rn;
        self.prev_x = x;
        self.settled.clear();
        self.vari.push(coherent + step, j, &mut self.settled);
        self.take_settled(now, out);
    }

    /// Pass what the Varicode search has settled on to the squelch.
    fn take_settled(&mut self, now: u64, out: &mut Vec<PskText>) {
        for i in 0..self.settled.len() {
            let Settled { ch, start, end } = self.settled[i];
            let n = (end + 1).saturating_sub(start).clamp(1, SYM_RING as u64 / 2);
            let quality = (end + 1 - n..=end).map(|s| self.m_hist[(s % SYM_RING as u64) as usize]).sum::<f32>() / n as f32;
            self.decoded(ch, end, quality, now, out);
        }
        self.settled.clear();
    }

    fn squelch(&mut self, now: u64, out: &mut Vec<PskText>) {
        if self.q_short > SQ_SHORT_GOOD {
            self.last_good = self.symbols;
        }
        // A character is printed once it is old enough and the phase steps
        // were plainly clean over a stretch wholly after it. While a signal
        // is there that keeps happening; once it has gone, what noise
        // decodes waits until the long average shuts the squelch and
        // throws it away.
        let good = self.last_good.saturating_sub(GOOD_AFTER);
        if self.open {
            if self.q < SQ_CLOSE {
                // The search's tentative tail is read out first.
                self.settled.clear();
                self.vari.flush(&mut self.settled);
                self.take_settled(now, out);
                // What nothing after it vouched for is kept as far as its
                // own bits came in clean enough: the last words of a weak
                // over often fade out before anything after them can
                // vouch, while the noise after an over seldom passes.
                self.commit_while(|h| h.symbol <= good, now, out);
                let rescue = self.last_good + 2;
                self.commit_while(|h| h.quality >= HELD_QUALITY && h.symbol <= rescue, now, out);
                self.open = false;
                self.n_held = 0;
                // The over has ended: its last line need not wait.
                self.emit(out);
            }
        } else if self.q > SQ_OPEN && self.q_short > SQ_SHORT_GOOD && self.symbols > 8 {
            self.open = true;
            self.open_since = self.symbols;
            self.fresh = true;
            self.opened_at.get_or_insert(self.symbols);
        }
        if self.open {
            let due = self.symbols.saturating_sub(COMMIT_DELAY).min(good);
            self.commit_while(|h| h.symbol <= due, now, out);
        }
    }

    /// Print what is pending as the squelch would at a close: the search's
    /// tail, then whatever is vouched for or came in clean, then the line.
    fn finish(&mut self, now: u64, out: &mut Vec<PskText>) {
        if self.open {
            self.settled.clear();
            self.vari.flush(&mut self.settled);
            self.take_settled(now, out);
            let good = self.last_good.saturating_sub(GOOD_AFTER);
            self.commit_while(|h| h.symbol <= good, now, out);
            let rescue = self.last_good + 2;
            self.commit_while(|h| h.quality >= HELD_QUALITY && h.symbol <= rescue, now, out);
            self.n_held = 0;
        }
        self.emit(out);
    }

    fn commit_while(&mut self, take: impl Fn(&Held) -> bool, now: u64, out: &mut Vec<PskText>) {
        let mut n = 0;
        while n < self.n_held && take(&self.held[n]) {
            let c = self.held[n].ch;
            self.print(c, now, out);
            n += 1;
        }
        if n > 0 {
            self.held.copy_within(n..self.n_held, 0);
            self.n_held -= n;
        }
    }

    fn decoded(&mut self, c: u8, end: u64, quality: f32, now: u64, out: &mut Vec<PskText>) {
        // Nothing that ended with the squelch shut is kept, not even just
        // before it opens: that is where the bit made of the noise before
        // a carrier and its first symbol lands, and it reads as a space.
        if !self.open {
            return;
        }
        if end < self.open_since {
            return;
        }
        if self.n_held == HELD_MAX {
            // A long open stretch with no plainly clean step fills the
            // queue: the oldest is printed rather than lost.
            let c0 = self.held[0].ch;
            self.print(c0, now, out);
            self.held.copy_within(1..HELD_MAX, 0);
            self.n_held -= 1;
        }
        self.held[self.n_held] = Held { ch: c, symbol: end, quality };
        self.n_held += 1;
    }

    fn print(&mut self, c: u8, now: u64, out: &mut Vec<PskText>) {
        self.chars += 1;
        self.last_char = now;
        match c {
            b'\n' | b'\r' => {
                self.emit(out);
                self.fresh = true;
            }
            0..=31 | 127 => {}
            // Text does not begin with a space after an opening or a line
            // end: that is idle broken by a bit error.
            b' ' if self.fresh => {}
            _ => {
                self.fresh = false;
                self.line.push(char::from(c));
                // Only ASCII is decoded, so bytes are characters.
                if self.line.len() >= LINE_MAX {
                    self.emit(out);
                }
            }
        }
    }

    fn emit(&mut self, out: &mut Vec<PskText>) {
        // A line of nothing but spaces (idle broken by a bit error or two
        // reads as spaces) says nothing.
        if self.line.trim().is_empty() {
            self.line.clear();
            return;
        }
        out.push(PskText {
            channel: self.id,
            audio_hz: self.hz(),
            snr_db: self.snr_db,
            text: std::mem::replace(&mut self.line, String::with_capacity(LINE_MAX + 8)),
        });
    }
}

/// A new channel started on the audio already heard, so it has the
/// transmission's opening words and its loops have settled by now.
///
/// For a weak signal the detector's frequency can be a few hertz out, and
/// the loop took seconds to pull it in: seconds that were the opening
/// words. So the carrier is found in the heard audio first. Squaring the
/// signal strips the modulation and leaves a line at twice the carrier's
/// offset, which three seconds of signal make stand well clear of the
/// noise even at -12 dB in 2.5 kHz. Idle adds lines a baud either side of
/// it, a quarter as strong; the matched filter is left out here, since it
/// weakens the true line once the carrier is off center and let a side
/// line win, which put channels half a baud off with garbage for text.
fn replayed(
    (id, hz, snr_db, last_char, now): (u32, f32, f32, u64, u64),
    history: &[f32],
    xc: &mut Vec<Complex32>,
    scratch: &mut Scratch,
    sh: &Shape,
    out: &mut Vec<PskText>,
) -> Channel {
    xc.clear();
    xc.extend(history.iter().map(|&v| Complex32::new(v, 0.0)));
    let mut probe = Channel::new(id, hz, snr_db, last_char, sh);
    probe.nco.mix(xc, &mut scratch.mixed);
    probe.fir.process(&scratch.mixed, &mut scratch.dec);
    let n = scratch.dec.len();
    let mut start_hz = hz;
    if n > 8 * sh.mf.len() {
        let len = n.next_power_of_two();
        let mut sq = vec![Complex32::new(0.0, 0.0); len];
        for (v, &y) in sq.iter_mut().zip(&scratch.dec) {
            *v = y * y;
        }
        FftPlanner::new().plan_fft_forward(len).process(&mut sq);
        let bin = sh.fs2 / len as f32;
        // Twice the offset, within the carrier loop's reach.
        let reach = ((2.0 * AFC_RANGE_HZ / bin) as usize).min(len / 2 - 2);
        let power = |k: isize| sq[k.rem_euclid(len as isize) as usize].norm_sqr();
        let (mut best, mut best_p) = (0isize, 0.0f32);
        let mut sum = 0.0f32;
        for k in -(reach as isize)..=reach as isize {
            let p = power(k);
            sum += p;
            if p > best_p {
                best_p = p;
                best = k;
            }
        }
        let mean = sum / (2 * reach + 1) as f32;
        // Only a line that plainly stands out is a carrier.
        if best_p > 12.0 * mean {
            let (a, b, c) = (power(best - 1), best_p, power(best + 1));
            let den = a - 2.0 * b + c;
            let off = if den < 0.0 { (0.5 * (a - c) / den).clamp(-0.5, 0.5) } else { 0.0 };
            start_hz = hz + 0.5 * (best as f32 + off) * bin;
        }
    }
    let mut ch = Channel::new(id, start_hz, snr_db, last_char, sh);
    ch.home_hz = hz;
    ch.carrier_found = start_hz != hz;
    ch.process(xc, scratch, sh, now, out);
    ch
}

/// Buffers every channel uses in turn.
struct Scratch {
    mixed: Vec<Complex32>,
    dec: Vec<Complex32>,
}

#[derive(Clone, Copy)]
struct Candidate {
    hz: f32,
    /// Mean power above the noise per bin over plus and minus one baud.
    excess: f32,
    snr_db: f32,
}

pub struct PskDecoder {
    shape: Shape,
    low_hz: f32,
    high_hz: f32,
    max_channels: usize,

    fe_decim: usize,
    fe_taps: Vec<f32>,
    fe_hist: Vec<f32>,
    x1: Vec<f32>,
    xc: Vec<Complex32>,
    /// The shared stream's last [`REPLAY_S`] (up to twice that before it
    /// is trimmed), oldest first.
    history: Vec<f32>,
    history_len: usize,

    fft: Arc<dyn Fft<f32>>,
    fft_len: usize,
    window: Vec<f32>,
    spec_ring: Vec<f32>,
    spec_pos: usize,
    since_hop: usize,
    fft_buf: Vec<Complex32>,
    fft_scratch: Vec<Complex32>,
    avg: Vec<f32>,
    frames: u32,
    frames_since_detect: u32,
    floor: Vec<f32>,
    excess: Vec<f32>,
    prefix: Vec<f32>,
    sorted: Vec<f32>,
    kernel: Vec<f32>,
    score: Vec<f32>,
    idle_won: Vec<bool>,
    taken: Vec<bool>,
    cands: Vec<Candidate>,

    channels: Vec<Channel>,
    next_id: u32,
    /// Shared-rate samples since the start.
    now: u64,
    scratch: Scratch,
    out: Vec<PskText>,
}

impl PskDecoder {
    /// A decoder for `fs` Hz audio at `baud` (31.25 or 62.5).
    pub fn new(fs: f32, baud: f32) -> Self {
        let fe_decim = ((fs / FRONT_MIN_RATE).floor() as usize).max(1);
        let fs1 = fs / fe_decim as f32;
        let fe_taps = if fe_decim > 1 {
            // Cut at half the new rate; the transition runs from the top of
            // the passband to its mirror image, the first thing to fold.
            let width = (fs1 - 2.0 * FRONT_PASS_HZ).max(0.1 * fs1);
            let n = (((8.0 * fs / width).ceil() as usize) | 1).clamp(31, 511);
            lowpass(0.5 * fs1, fs, n)
        } else {
            Vec::new()
        };
        // Bins of about 2 Hz: the idle lines of BPSK31 sit 31 Hz apart and
        // need several bins between them.
        let fft_len = ((fs1 / 2.0) as usize).next_power_of_two().max(256);
        let fft = FftPlanner::new().plan_fft_forward(fft_len);
        let window: Vec<f32> = (0..fft_len).map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / fft_len as f32).cos()).collect();
        let shape = Shape::new(fs1, baud);
        let history_len = (REPLAY_S * fs1) as usize;
        let zero = Complex32::new(0.0, 0.0);
        let mut this = Self {
            shape,
            low_hz: 200.0,
            high_hz: 3000.0,
            max_channels: 16,
            fe_decim,
            fe_taps,
            fe_hist: Vec::new(),
            x1: Vec::new(),
            xc: Vec::new(),
            history: Vec::with_capacity(2 * history_len + 4096),
            history_len,
            fft_scratch: vec![zero; fft.get_inplace_scratch_len()],
            fft,
            fft_len,
            window,
            spec_ring: vec![0.0; fft_len],
            spec_pos: 0,
            since_hop: 0,
            fft_buf: vec![zero; fft_len],
            avg: vec![0.0; fft_len / 2],
            frames: 0,
            frames_since_detect: 0,
            floor: Vec::new(),
            excess: Vec::new(),
            prefix: Vec::new(),
            sorted: Vec::new(),
            kernel: Vec::new(),
            score: Vec::new(),
            idle_won: Vec::new(),
            taken: Vec::new(),
            cands: Vec::new(),
            channels: Vec::new(),
            next_id: 1,
            now: 0,
            scratch: Scratch { mixed: Vec::new(), dec: Vec::new() },
            out: Vec::new(),
        };
        this.set_band(200.0, 3000.0);
        this
    }

    /// The audio searched for signals. The top is held below what the
    /// shared filter keeps clean.
    pub fn set_band(&mut self, low_hz: f32, high_hz: f32) {
        let b = self.shape.baud;
        let top = (FRONT_PASS_HZ.min(0.5 * self.shape.fs1) - 1.5 * b).max(4.0 * b);
        self.low_hz = low_hz.clamp(2.0 * b, top - 2.0 * b);
        self.high_hz = high_hz.clamp(self.low_hz + 2.0 * b, top);
    }

    pub fn set_max_channels(&mut self, n: usize) {
        self.max_channels = n.max(1);
    }

    pub fn push(&mut self, audio: &[f32]) {
        // Pieces of about 50 ms, so the detector and the line timers run on
        // time however large the caller's blocks are.
        let piece = (self.fe_decim * 400).max(1);
        for chunk in audio.chunks(piece) {
            self.push_piece(chunk);
        }
    }

    /// Decoded text since the last call.
    pub fn take(&mut self) -> Vec<PskText> {
        std::mem::take(&mut self.out)
    }

    pub fn channels(&self) -> Vec<PskChannel> {
        self.channels
            .iter()
            .map(|c| PskChannel { id: c.id, audio_hz: c.hz(), snr_db: c.snr_db, active: c.open, chars: c.chars })
            .collect()
    }

    fn push_piece(&mut self, audio: &[f32]) {
        self.front_end(audio);
        if self.x1.is_empty() {
            return;
        }
        self.xc.clear();
        self.xc.extend(self.x1.iter().map(|&v| Complex32::new(v, 0.0)));
        self.history.extend_from_slice(&self.x1);
        if self.history.len() > 2 * self.history_len {
            let cut = self.history.len() - self.history_len;
            self.history.drain(..cut);
        }
        let now = self.now + self.x1.len() as u64;
        for ch in &mut self.channels {
            ch.process(&self.xc, &mut self.scratch, &self.shape, now, &mut self.out);
        }
        self.now = now;
        self.spectrum();
        if self.frames_since_detect >= DETECT_EVERY && self.frames >= MIN_FRAMES {
            self.frames_since_detect = 0;
            self.detect();
        }
        let flush = (FLUSH_S * self.shape.fs1) as u64;
        for ch in &mut self.channels {
            if !ch.line.is_empty() && now.saturating_sub(ch.last_char) >= flush {
                ch.emit(&mut self.out);
            }
        }
    }

    fn front_end(&mut self, audio: &[f32]) {
        self.x1.clear();
        if self.fe_decim == 1 {
            self.x1.extend_from_slice(audio);
            return;
        }
        self.fe_hist.extend_from_slice(audio);
        let n = self.fe_taps.len();
        let mut i = 0;
        while i + n <= self.fe_hist.len() {
            self.x1.push(dot(&self.fe_hist[i..i + n], &self.fe_taps));
            i += self.fe_decim;
        }
        // The taps outnumber the decimation, so `i` never passes the end.
        self.fe_hist.drain(..i.min(self.fe_hist.len()));
    }

    fn spectrum(&mut self) {
        let hop = self.fft_len / SPEC_OVERLAP;
        let alpha0 = hop as f32 / (SPEC_TAU_S * self.shape.fs1);
        for k in 0..self.x1.len() {
            self.spec_ring[self.spec_pos] = self.x1[k];
            self.spec_pos = (self.spec_pos + 1) % self.fft_len;
            self.since_hop += 1;
            if self.since_hop < hop {
                continue;
            }
            self.since_hop = 0;
            let n = self.fft_len;
            for j in 0..n {
                let v = self.spec_ring[(self.spec_pos + j) % n];
                self.fft_buf[j] = Complex32::new(v * self.window[j], 0.0);
            }
            self.fft.process_with_scratch(&mut self.fft_buf, &mut self.fft_scratch);
            self.frames += 1;
            self.frames_since_detect += 1;
            // A plain mean until the average is as long as its time
            // constant, so the first look is not dominated by one frame.
            let alpha = (1.0 / self.frames as f32).max(alpha0);
            for (a, x) in self.avg.iter_mut().zip(&self.fft_buf) {
                *a += (x.norm_sqr() - *a) * alpha;
            }
        }
    }

    /// Find PSK signals in the averaged spectrum, then match them to the
    /// channels, start channels for new ones, and drop stale channels.
    fn detect(&mut self) {
        self.find_candidates();
        let baud = self.shape.baud;
        let now = self.now;
        let fs1 = self.shape.fs1;
        let cands = std::mem::take(&mut self.cands);
        for cand in &cands {
            if let Some(ch) = self.channels.iter_mut().find(|c| (c.hz() - cand.hz).abs() < 0.75 * baud) {
                ch.last_seen = now;
                ch.snr_db = cand.snr_db;
                if !ch.open && (cand.hz - ch.hz()).abs() > 3.0 {
                    // Not copying, and the signal is elsewhere: go to it
                    // rather than wait for the loop to wander there. A
                    // channel that has never copied starts over there on
                    // the audio heard so far, so a first guess that fell
                    // between two signals does not cost the opening words.
                    if ch.opened_at.is_none() {
                        let start = self.history.len().saturating_sub(self.history_len);
                        let mut fresh = replayed(
                            (ch.id, cand.hz, cand.snr_db, ch.last_char, now),
                            &self.history[start..],
                            &mut self.xc,
                            &mut self.scratch,
                            &self.shape,
                            &mut self.out,
                        );
                        fresh.last_seen = now;
                        *ch = fresh;
                    } else {
                        ch.home_hz = cand.hz;
                        ch.recenter(cand.hz, &self.shape);
                    }
                } else {
                    ch.home_hz = ch.hz();
                    if ch.fine_hz.abs() > 2.0 {
                        let hz = ch.hz();
                        ch.recenter(hz, &self.shape);
                    }
                }
                continue;
            }
            if self.channels.iter().any(|c| (c.hz() - cand.hz).abs() < baud) {
                continue;
            }
            if self.channels.len() >= self.max_channels {
                // Make room only by dropping an idle channel weaker than
                // the newcomer.
                let evict = (EVICT_IDLE_S * fs1) as u64;
                let victim = self
                    .channels
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| now.saturating_sub(c.last_char) >= evict && c.snr_db < cand.snr_db)
                    .min_by(|a, b| a.1.snr_db.total_cmp(&b.1.snr_db))
                    .map(|(i, _)| i);
                match victim {
                    Some(i) => self.retire(i),
                    None => continue,
                }
            }
            let start = self.history.len().saturating_sub(self.history_len);
            let ch = replayed(
                (self.next_id, cand.hz, cand.snr_db, now, now),
                &self.history[start..],
                &mut self.xc,
                &mut self.scratch,
                &self.shape,
                &mut self.out,
            );
            self.next_id += 1;
            self.channels.push(ch);
        }
        self.cands = cands;

        // Two channels on one signal: keep the one that has printed more.
        let mut i = 0;
        while i < self.channels.len() {
            let dup = (0..self.channels.len()).find(|&j| {
                j != i && (self.channels[i].hz() - self.channels[j].hz()).abs() < 0.6 * baud && {
                    let (a, b) = (&self.channels[i], &self.channels[j]);
                    (a.chars, std::cmp::Reverse(a.id)) < (b.chars, std::cmp::Reverse(b.id))
                }
            });
            if dup.is_some() {
                self.retire(i);
            } else {
                i += 1;
            }
        }

        let idle = (RETIRE_IDLE_S * fs1) as u64;
        let unseen = (RETIRE_UNSEEN_S * fs1) as u64;
        let hard = (RETIRE_HARD_S * fs1) as u64;
        let (lo, hi) = (self.low_hz - baud, self.high_hz + baud);
        let mut i = 0;
        while i < self.channels.len() {
            let c = &self.channels[i];
            let quiet = now.saturating_sub(c.last_char);
            let stale = (quiet >= idle && now.saturating_sub(c.last_seen) >= unseen) || quiet >= hard;
            if stale || c.hz() < lo || c.hz() > hi {
                self.retire(i);
            } else {
                i += 1;
            }
        }
        while self.channels.len() > self.max_channels {
            let i = (0..self.channels.len()).min_by_key(|&i| self.channels[i].last_char).unwrap_or(0);
            self.retire(i);
        }
    }

    fn retire(&mut self, i: usize) {
        let mut ch = self.channels.remove(i);
        ch.finish(self.now, &mut self.out);
    }

    fn find_candidates(&mut self) {
        self.cands.clear();
        let bin_hz = self.shape.fs1 / self.fft_len as f32;
        let nb = self.avg.len();
        let w = ((self.shape.baud / bin_hz).round() as usize).max(2);
        let klo = ((self.low_hz / bin_hz).ceil() as usize).max(w + 1);
        let khi = ((self.high_hz / bin_hz).floor() as usize).min(nb - w - 2);
        if khi <= klo {
            return;
        }
        let (a, b) = (klo - w, khi + w + 1);

        // Noise floor per segment: the mean of the bins below twice the
        // 30th percentile. Signals sit above that cut and drop out, and for
        // noise alone the trimmed mean is within a few percent of the mean.
        self.floor.clear();
        self.floor.resize(nb, 0.0);
        let seg = ((FLOOR_SEG_HZ / bin_hz) as usize).max(8);
        let nseg = (b - a).div_ceil(seg);
        let mut centers: [(f32, f32); 32] = [(0.0, 0.0); 32];
        let nseg = nseg.min(32);
        let seg = (b - a).div_ceil(nseg);
        for (s, center) in centers.iter_mut().enumerate().take(nseg) {
            let s0 = (a + s * seg).min(b - 1);
            let s1 = (s0 + seg).min(b);
            self.sorted.clear();
            self.sorted.extend_from_slice(&self.avg[s0..s1]);
            let k = self.sorted.len() * 3 / 10;
            let (_, p30, _) = self.sorted.select_nth_unstable_by(k, f32::total_cmp);
            let cut = 2.0 * *p30;
            let (sum, cnt) = self.avg[s0..s1].iter().filter(|&&v| v < cut).fold((0.0f32, 0u32), |(s, n), &v| (s + v, n + 1));
            let f = if cnt > 0 { sum / cnt as f32 } else { cut };
            *center = (0.5 * (s0 + s1) as f32, f.max(1e-30));
        }
        for k in a..b {
            let x = k as f32;
            let j = centers[..nseg].partition_point(|c| c.0 <= x);
            self.floor[k] = if j == 0 {
                centers[0].1
            } else if j >= nseg {
                centers[nseg - 1].1
            } else {
                let (x0, f0) = centers[j - 1];
                let (x1, f1) = centers[j];
                f0 + (f1 - f0) * (x - x0) / (x1 - x0)
            };
        }
        self.excess.clear();
        self.excess.resize(nb, 0.0);
        for k in a..b {
            self.excess[k] = self.avg[k] / self.floor[k] - 1.0;
        }
        self.prefix.clear();
        self.prefix.push(0.0);
        let mut run = 0.0f32;
        for &x in &self.excess {
            run += x;
            self.prefix.push(run);
        }
        let win = |c: usize| self.prefix[c + w + 1] - self.prefix[c - w];
        // Power within two bins of `f`, as a share of the power within
        // five: a line has nearly all of it in the inner two.
        let line = |f: usize| {
            let inner = self.prefix[f + 3] - self.prefix[f - 2];
            let outer = self.prefix[(f + 6).min(nb)] - self.prefix[f.saturating_sub(5)];
            if inner >= 0.7 * outer { inner } else { 0.0 }
        };
        let span = (2 * w + 1) as f32;

        // Where the signals are, from two narrow scores: the power under a
        // half-baud raised-cosine bump (traffic, whose spectrum peaks at
        // the carrier), and twice the weaker of two lines half a baud
        // either side (idle). The window sum is flat for as far as both
        // idle lines fit in it, and two signals 50 Hz apart peak it
        // between them; these peak on each carrier. The lines must be
        // narrow: the inner flanks of two such neighbors' traffic are
        // otherwise a fair imitation of an idle pair between them.
        let h = (w / 2).max(1);
        if self.kernel.len() != 2 * h + 1 {
            self.kernel = (0..=2 * h)
                .map(|j| (PI * (j as f32 - h as f32) / (2 * h + 2) as f32).cos().powi(2))
                .collect();
        }
        self.score.clear();
        self.score.resize(nb, 0.0);
        self.idle_won.clear();
        self.idle_won.resize(nb, false);
        for c in klo..=khi {
            let bump = dot(&self.excess[c - h..=c + h], &self.kernel);
            let idle = 2.0 * line(c - h).min(line(c + h));
            self.score[c] = bump.max(idle);
            self.idle_won[c] = idle > bump;
        }
        self.taken.clear();
        self.taken.resize(nb, false);
        let limit = 2 * self.max_channels + 8;
        for _ in 0..8 * limit {
            if self.cands.len() >= limit {
                break;
            }
            let mut best = None;
            let mut best_s = 0.0f32;
            for c in klo..=khi {
                if !self.taken[c] && self.score[c] > best_s {
                    best_s = self.score[c];
                    best = Some(c);
                }
            }
            let Some(c) = best else { break };
            for t in &mut self.taken[c - h..=c + h] {
                *t = true;
            }
            // A signal is the highest score within a baud either side. The
            // shoulders of a strong signal a little over a baud out score
            // well, but always below the signal itself.
            let (n0, n1) = (c.saturating_sub(w).max(klo), (c + w).min(khi));
            if self.score[n0..=n1].iter().any(|&v| v > best_s) {
                continue;
            }
            let s = win(c);
            if s < DETECT_EXCESS * span {
                continue;
            }
            if !self.idle_won[c] {
                // A carrier puts nearly all of its power within two bins of
                // its peak; PSK traffic spreads it over the window.
                let total: f32 = self.excess[c - w..=c + w].iter().map(|x| x.max(0.0)).sum();
                let peak = (c - h..=c + h).max_by(|&i, &j| self.excess[i].total_cmp(&self.excess[j])).unwrap_or(c);
                let near: f32 = self.excess[peak - 2..=peak + 2].iter().map(|x| x.max(0.0)).sum();
                if total <= 0.0 || near > CW_SHARE * total {
                    for t in &mut self.taken[c - w..=c + w] {
                        *t = true;
                    }
                    continue;
                }
            }
            // Between bins, from a parabola through the peak and its
            // neighbors.
            let (l, m, r) = (self.score[c - 1], self.score[c], self.score[c + 1]);
            let den = l - 2.0 * m + r;
            let off = if den < 0.0 { (0.5 * (l - r) / den).clamp(-0.5, 0.5) } else { 0.0 };
            let hz = (c as f32 + off) * bin_hz;
            let close = self.cands.iter().any(|o| {
                let d = (o.hz - hz).abs();
                d < self.shape.baud || (d < 4.0 * self.shape.baud && o.excess > SIDELOBE_RATIO * s / span)
            });
            if close {
                continue;
            }
            let snr = s * bin_hz / 2500.0;
            self.cands.push(Candidate { hz, excess: s / span, snr_db: 10.0 * snr.max(1e-6).log10() });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    const FS: f32 = 48_000.0;

    /// xorshift64*, for noise that is the same on every run.
    struct Rng(u64);
    impl Rng {
        fn uniform(&mut self) -> f64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            (self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64 / (1u64 << 53) as f64
        }
        fn gauss(&mut self) -> f64 {
            let u = self.uniform().max(1e-300);
            let v = self.uniform();
            (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
        }
    }

    /// One transmission: a second of idle, the text in Varicode, a second
    /// of idle, the carrier raised and lowered on a half-cosine.
    struct Tx {
        hz: f64,
        drift_hz_per_s: f64,
        amp: f64,
        baud: f64,
        start_s: f64,
        text: String,
        fading: Option<Fading>,
    }

    /// A Watterson HF channel: two paths of equal mean power, each
    /// Rayleigh-faded with a Gaussian Doppler spectrum whose two-sigma
    /// width is the spread, the second delayed. CCIR moderate is 1 ms and
    /// 0.5 Hz, poor 2 ms and 1 Hz.
    #[derive(Clone, Copy)]
    struct Fading {
        delay_s: f64,
        spread_hz: f64,
        seed: u64,
    }

    const MODERATE: Fading = Fading { delay_s: 1e-3, spread_hz: 0.5, seed: 21 };
    const POOR: Fading = Fading { delay_s: 2e-3, spread_hz: 1.0, seed: 22 };

    impl Fading {
        /// One path's complex gain at `rate` Hz for `n` samples, mean power
        /// one half: white noise shaped by the Doppler spectrum.
        fn taps(&self, n: usize, rate: f64, seed: u64) -> Vec<num_complex::Complex64> {
            use num_complex::Complex64;
            let len = n.next_power_of_two().max(1024);
            let mut rng = Rng(seed | 1);
            let sigma = self.spread_hz / 2.0;
            let mut x: Vec<Complex64> = (0..len)
                .map(|k| {
                    let f = if k <= len / 2 { k as f64 } else { k as f64 - len as f64 } * rate / len as f64;
                    let g = (-f * f / (4.0 * sigma * sigma)).exp();
                    Complex64::new(rng.gauss() * g, rng.gauss() * g)
                })
                .collect();
            rustfft::FftPlanner::new().plan_fft_inverse(len).process(&mut x);
            let p = x.iter().map(|v| v.norm_sqr()).sum::<f64>() / len as f64;
            let k = (0.5 / p).sqrt();
            x.truncate(n);
            x.iter_mut().for_each(|v| *v *= k);
            x
        }
    }

    fn bits(text: &str, baud: f64) -> Vec<bool> {
        let idle = baud as usize;
        let mut b = vec![false; idle];
        for c in text.bytes() {
            b.extend(VARICODE[c as usize].bytes().map(|x| x == b'1'));
            b.extend([false, false]);
        }
        b.extend(std::iter::repeat_n(false, idle));
        b
    }

    impl Tx {
        fn new(hz: f64, amp: f64, text: &str) -> Self {
            Self { hz, drift_hz_per_s: 0.0, amp, baud: 31.25, start_s: 0.5, text: text.to_string(), fading: None }
        }
        fn duration_s(&self) -> f64 {
            (bits(&self.text, self.baud).len() + 2) as f64 / self.baud
        }
        /// Add the signal into `out` (48 kHz); returns its mean power over
        /// the time it is on.
        fn render(&self, out: &mut [f32]) -> f64 {
            let b = bits(&self.text, self.baud);
            // Symbol values: a reversal for each 0. The zero before and
            // after shape the carrier's rise and fall.
            let mut a = vec![0.0f64];
            let mut cur = 1.0f64;
            for &bit in &b {
                if !bit {
                    cur = -cur;
                }
                a.push(cur);
            }
            a.push(0.0);
            let fs = FS as f64;
            let s0 = (self.start_s * fs) as usize;
            let n = ((a.len() - 1) as f64 / self.baud * fs) as usize;
            let env: Vec<f64> = (0..n)
                .map(|i| {
                    let pos = i as f64 / fs * self.baud;
                    let k = (pos as usize).min(a.len() - 2);
                    let u = pos - k as f64;
                    0.5 * (a[k] + a[k + 1]) + 0.5 * (a[k] - a[k + 1]) * (std::f64::consts::PI * u).cos()
                })
                .collect();
            // Path gains at 100 Hz, interpolated; far slower than any
            // Doppler spread here.
            const TAP_RATE: f64 = 100.0;
            let fade = self.fading.map(|f| {
                let m = (n as f64 / fs * TAP_RATE) as usize + 2;
                (f.taps(m, TAP_RATE, f.seed), f.taps(m, TAP_RATE, f.seed.wrapping_mul(7919)), (f.delay_s * fs) as usize)
            });
            let mut phase = 0.3f64;
            let mut pw = 0.0;
            for i in 0..n {
                if s0 + i >= out.len() {
                    break;
                }
                let t = i as f64 / fs;
                let f = self.hz + self.drift_hz_per_s * t;
                phase += 2.0 * std::f64::consts::PI * f / fs;
                let (c, s) = (phase.cos(), phase.sin());
                // Mean power is that of the unfaded signal: the SNR of a
                // faded signal is its average.
                pw += (self.amp * env[i] * c).powi(2);
                let v = match &fade {
                    None => self.amp * env[i] * c,
                    Some((g1, g2, d)) => {
                        let x = t * TAP_RATE;
                        let j = x as usize;
                        let w = x - j as f64;
                        let a1 = g1[j] * (1.0 - w) + g1[j + 1] * w;
                        let a2 = g2[j] * (1.0 - w) + g2[j + 1] * w;
                        let e2 = if i >= *d { env[i - d] } else { 0.0 };
                        let z = a1 * env[i] + a2 * e2;
                        self.amp * (z.re * c - z.im * s)
                    }
                };
                out[s0 + i] += v as f32;
            }
            pw / n as f64
        }
    }

    /// Noise of the given power per sample (flat to 24 kHz).
    fn add_noise(out: &mut [f32], power: f64, seed: u64) {
        let mut rng = Rng(seed | 1);
        let s = power.sqrt();
        for v in out {
            *v += (rng.gauss() * s) as f32;
        }
    }

    /// Noise power per sample that puts a signal of `sig_power` at
    /// `snr_db` in 3 kHz: 3 kHz is an eighth of the 24 kHz the noise covers.
    fn noise_for(sig_power: f64, snr_db: f64) -> f64 {
        sig_power / 10f64.powf(snr_db / 10.0) * (FS as f64 / 2.0) / 3000.0
    }

    /// The same for an SNR in 2500 Hz.
    fn noise_for_2500(sig_power: f64, snr_db: f64) -> f64 {
        sig_power / 10f64.powf(snr_db / 10.0) * (FS as f64 / 2.0) / 2500.0
    }

    struct Run {
        texts: Vec<PskText>,
        secs: f64,
        audio_s: f64,
    }

    fn run(dec: &mut PskDecoder, audio: &[f32]) -> Run {
        let mut texts = Vec::new();
        let t0 = Instant::now();
        for chunk in audio.chunks(960) {
            dec.push(chunk);
            texts.extend(dec.take());
        }
        Run { texts, secs: t0.elapsed().as_secs_f64(), audio_s: audio.len() as f64 / FS as f64 }
    }

    /// All text from the channel nearest `hz`, joined.
    fn text_near(texts: &[PskText], hz: f32) -> (Option<u32>, String) {
        let id = texts.iter().filter(|t| (t.audio_hz - hz).abs() < 12.0).map(|t| t.channel).next();
        let s = texts.iter().filter(|t| Some(t.channel) == id).map(|t| t.text.as_str()).collect::<String>();
        (id, s)
    }

    fn levenshtein(a: &str, b: &str) -> usize {
        let a: Vec<char> = a.chars().collect();
        let b: Vec<char> = b.chars().collect();
        let mut prev: Vec<usize> = (0..=b.len()).collect();
        let mut cur = vec![0; b.len() + 1];
        for i in 1..=a.len() {
            cur[0] = i;
            for j in 1..=b.len() {
                let sub = prev[j - 1] + usize::from(a[i - 1] != b[j - 1]);
                cur[j] = sub.min(prev[j] + 1).min(cur[j - 1] + 1);
            }
            std::mem::swap(&mut prev, &mut cur);
        }
        prev[b.len()]
    }

    fn cer(want: &str, got: &str) -> f64 {
        levenshtein(want, got) as f64 / want.chars().count() as f64
    }

    fn audio_for(txs: &[Tx], extra_s: f64) -> (Vec<f32>, Vec<f64>) {
        let dur = txs.iter().map(|t| t.start_s + t.duration_s()).fold(0.0, f64::max) + extra_s;
        let mut out = vec![0.0f32; (dur * FS as f64) as usize];
        let p = txs.iter().map(|t| t.render(&mut out)).collect();
        (out, p)
    }

    #[test]
    fn psk_varicode_table_is_well_formed() {
        let mut seen = std::collections::HashSet::new();
        for code in VARICODE {
            assert!(code.starts_with('1') && code.ends_with('1') && !code.contains("00"), "{code}");
            assert!(code.len() <= 10);
            assert!(seen.insert(code), "duplicate {code}");
        }
    }

    #[test]
    fn psk_single_signal_round_trip() {
        let text = "CQ CQ CQ DE N0CALL N0CALL K";
        let tx = Tx::new(1500.0, 0.02, text);
        let (mut audio, p) = audio_for(&[tx], 4.0);
        add_noise(&mut audio, noise_for(p[0], 10.0), 1);
        let mut dec = PskDecoder::new(FS, 31.25);
        let r = run(&mut dec, &audio);
        let (id, got) = text_near(&r.texts, 1500.0);
        println!("single: {id:?} {got:?} {:?}", dec.channels());
        assert_eq!(got, text);
    }

    #[test]
    fn psk_five_signals_at_once() {
        let spec = [
            (600.0, -6.0, "CQ DX DE W1AW W1AW PSE K"),
            (900.0, 0.0, "TNX FOR THE QSO ES 73 DE K2ABC"),
            (1000.0, -12.0, "rig is an ic-7300 at 50 watts into a dipole"),
            (1800.0, -18.0, "UR RST 599 599 NAME IS JOE QTH OHIO"),
            (2600.0, -3.0, "test de ve3xyz ve3xyz the quick brown fox"),
        ];
        let txs: Vec<Tx> = spec
            .iter()
            .enumerate()
            .map(|(i, &(hz, db, text))| {
                let mut t = Tx::new(hz, 0.1 * 10f64.powf(db / 20.0), text);
                t.start_s = 0.3 + 0.7 * i as f64;
                t
            })
            .collect();
        let (mut audio, p) = audio_for(&txs, 4.0);
        // The weakest at 0 dB in 3 kHz.
        add_noise(&mut audio, noise_for(p[3], 0.0), 2);
        let mut dec = PskDecoder::new(FS, 31.25);
        let r = run(&mut dec, &audio);
        let mut ids = Vec::new();
        for &(hz, db, text) in &spec {
            let (id, got) = text_near(&r.texts, hz as f32);
            println!("{hz} Hz {db} dB: {id:?} {got:?}");
            assert_eq!(got, text, "at {hz} Hz");
            ids.push(id);
        }
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 5);
    }

    const QSO: &str = "W1AW de K2ABC, good morning and thanks for the call. Your signal is 579 with some fading here in Albany. \
        The name here is Pat and the rig is a small transceiver at 30 watts into an end-fed wire. \
        Weather is cold and gray with light snow. How copy? W1AW de K2ABC kn ";

    /// Character error rate for a long text at `snr_db` in 3 kHz.
    fn cer_at(snr_db: f64, seed: u64) -> f64 {
        let text = QSO.repeat(4);
        let tx = Tx::new(1234.0, 3e-4, &text);
        let (mut audio, p) = audio_for(&[tx], 4.0);
        add_noise(&mut audio, noise_for(p[0], snr_db), seed);
        let mut dec = PskDecoder::new(FS, 31.25);
        let r = run(&mut dec, &audio);
        let (_, got) = text_near(&r.texts, 1234.0);
        cer(&text, &got)
    }

    #[test]
    fn psk_poor_snr_character_error_rate() {
        let mut table = Vec::new();
        for snr in [-6.0, -9.0, -11.0, -13.0, -15.0] {
            let c = cer_at(snr, 7);
            println!("SNR {snr:>5.1} dB in 3 kHz ({:>5.1} dB in 2.5 kHz): CER {:.2}%", snr + 0.79, 100.0 * c);
            table.push((snr, c));
        }
        assert!(table[0].1 < 0.01, "{table:?}");
        assert!(table[1].1 < 0.03, "{table:?}");
        assert!(table[2].1 < 0.10, "{table:?}");
    }

    #[test]
    fn psk_tracks_offset_and_drift() {
        // The channel is put 10 Hz off and the signal drifts 2 Hz a minute.
        let text = QSO.to_string();
        let mut tx = Tx::new(1510.0, 0.01, &text);
        tx.drift_hz_per_s = 2.0 / 60.0;
        tx.start_s = 0.0;
        let (mut audio, p) = audio_for(&[tx], 1.0);
        add_noise(&mut audio, noise_for(p[0], 0.0), 3);
        let on_s = audio.len() as f32 / FS - 2.5;
        let (_, out, track) = run_channel(&audio, 1500.0, 31.25);
        let mut worst = 0.0f32;
        for (i, &hz) in track.iter().enumerate() {
            let t = (i + 1) as f32 * 960.0 / FS;
            if t > 3.0 && t < on_s {
                worst = worst.max((hz - (1510.0 + 2.0 / 60.0 * t)).abs());
            }
        }
        let got: String = out.iter().map(|t| t.text.as_str()).collect();
        println!("offset: worst tracking error {worst:.2} Hz after 3 s, CER {:.2}%, {got:?}", 100.0 * cer(&text, &got));
        assert!(worst < 1.0);
        assert!(cer(&text, &got) < 0.02, "{got:?}");

        // Through the detector, three minutes of drift keep one channel.
        let text = QSO.repeat(3);
        let mut tx = Tx::new(1300.0, 0.01, &text);
        tx.drift_hz_per_s = 2.0 / 60.0;
        let (mut audio, p) = audio_for(&[tx], 4.0);
        add_noise(&mut audio, noise_for(p[0], 0.0), 4);
        let mut dec = PskDecoder::new(FS, 31.25);
        let r = run(&mut dec, &audio);
        let mut ids: Vec<u32> = r.texts.iter().map(|t| t.channel).collect();
        ids.dedup();
        let got: String = r.texts.iter().map(|t| t.text.as_str()).collect();
        println!("drift: {:.0} s, channels {ids:?}, CER {:.2}%", r.audio_s, 100.0 * cer(&text, &got));
        assert_eq!(ids.len(), 1);
        assert!(cer(&text, &got) < 0.02);
    }

    #[test]
    fn psk63_round_trip() {
        let text = "CQ CQ DE N0CALL BPSK63 TEST 1234567890 K";
        let mut tx = Tx::new(1700.0, 0.05, text);
        tx.baud = 62.5;
        let (mut audio, p) = audio_for(&[tx], 4.0);
        add_noise(&mut audio, noise_for(p[0], 5.0), 5);
        let mut dec = PskDecoder::new(FS, 62.5);
        let r = run(&mut dec, &audio);
        let (_, got) = text_near(&r.texts, 1700.0);
        assert_eq!(got, text);
    }

    #[test]
    fn psk_noise_and_carrier_print_nothing() {
        let mut audio = vec![0.0f32; 60 * FS as usize];
        add_noise(&mut audio, 1e-4, 6);
        let mut dec = PskDecoder::new(FS, 31.25);
        let r = run(&mut dec, &audio);
        let n: usize = r.texts.iter().map(|t| t.text.chars().count()).sum();
        println!("noise: {n} chars, {} channels: {:?}", dec.channels().len(), r.texts);
        assert!(n <= 2);

        // A steady carrier 20 dB over the noise in 3 kHz.
        let mut audio = vec![0.0f32; 30 * FS as usize];
        for (i, v) in audio.iter_mut().enumerate() {
            *v = 0.05 * (2.0 * std::f32::consts::PI * 1200.0 * i as f32 / FS).cos();
        }
        let p = 0.05f64 * 0.05 / 2.0;
        add_noise(&mut audio, noise_for(p, 20.0), 8);
        let mut dec = PskDecoder::new(FS, 31.25);
        let r = run(&mut dec, &audio);
        println!("carrier: {:?} {:?}", r.texts, dec.channels());
        assert!(r.texts.is_empty());
        assert!(dec.channels().iter().all(|c| (c.audio_hz - 1200.0).abs() > 40.0));
    }

    /// Run one channel by itself over `audio`, sampling its frequency.
    fn run_channel(audio: &[f32], hz: f32, baud: f32) -> (Channel, Vec<PskText>, Vec<f32>) {
        let mut dec = PskDecoder::new(FS, baud);
        let mut ch = Channel::new(1, hz, 0.0, 0, &dec.shape);
        let mut out = Vec::new();
        let mut track = Vec::new();
        for chunk in audio.chunks(960) {
            dec.front_end(chunk);
            let xc: Vec<Complex32> = dec.x1.iter().map(|&v| Complex32::new(v, 0.0)).collect();
            dec.now += xc.len() as u64;
            ch.process(&xc, &mut dec.scratch, &dec.shape, dec.now, &mut out);
            track.push(ch.hz());
        }
        ch.finish(dec.now, &mut out);
        (ch, out, track)
    }

    #[test]
    fn psk_channel_on_noise_prints_nothing() {
        // A channel held on noise alone for a minute, at both rates.
        for (baud, seed) in [(31.25, 11), (62.5, 12)] {
            let mut audio = vec![0.0f32; 60 * FS as usize];
            add_noise(&mut audio, 0.3, seed);
            let (ch, out, _) = run_channel(&audio, 1500.0, baud);
            let got: String = out.iter().map(|t| t.text.as_str()).collect();
            println!("noise channel at {baud} baud: {} chars {got:?}, wandered to {:.1} Hz", ch.chars, ch.hz());
            assert!(ch.chars <= 2, "{got:?}");
        }
    }

    #[test]
    fn psk_neighbors_50_hz_apart() {
        let a = "CQ CQ DE AA1AA AA1AA K";
        let b = "QRZ? de bb2bb bb2bb kn";
        let mut t1 = Tx::new(1400.0, 0.01, a);
        t1.start_s = 0.4;
        let t2 = Tx::new(1450.0, 0.01, b);
        let (mut audio, p) = audio_for(&[t1, t2], 3.0);
        add_noise(&mut audio, noise_for(p[0], 3.0), 13);
        let mut dec = PskDecoder::new(FS, 31.25);
        let r = run(&mut dec, &audio);
        let (ia, ga) = text_near(&r.texts, 1400.0);
        let (ib, gb) = text_near(&r.texts, 1450.0);
        println!("50 Hz apart: {ia:?} {ga:?} / {ib:?} {gb:?}, CER {:.1}% {:.1}%", 100.0 * cer(a, &ga), 100.0 * cer(b, &gb));
        assert_ne!(ia, ib);
        assert!(cer(a, &ga) < 0.1 && cer(b, &gb) < 0.1);
    }

    #[test]
    fn psk_reports_snr() {
        // Read the channel list mid-transmission against the SNR the noise
        // was set for.
        for snr in [-10.0, 0.0, 10.0] {
            let tx = Tx::new(1500.0, 0.01, QSO);
            let (mut audio, p) = audio_for(&[tx], 0.0);
            add_noise(&mut audio, noise_for(p[0], snr), 14);
            let mut dec = PskDecoder::new(FS, 31.25);
            let half = audio.len() / 2;
            run(&mut dec, &audio[..half]);
            let ch = dec.channels();
            println!("SNR {snr} dB in 3 kHz ({:.1} in 2.5 kHz): reported {:?}", snr + 0.79, ch.iter().map(|c| c.snr_db).collect::<Vec<_>>());
            assert_eq!(ch.len(), 1);
            assert!((ch[0].snr_db - (snr as f32 + 0.79)).abs() < 2.0);
        }
    }

    #[test]
    fn psk_signal_end_prints_no_noise() {
        // A channel left on noise after its station stops must stay quiet.
        let text = "CQ CQ DE N0CALL K";
        let tx = Tx::new(1500.0, 0.01, text);
        let (mut audio, p) = audio_for(&[tx], 45.0);
        add_noise(&mut audio, noise_for(p[0], 0.0), 9);
        let mut dec = PskDecoder::new(FS, 31.25);
        let r = run(&mut dec, &audio);
        let got: String = r.texts.iter().map(|t| t.text.as_str()).collect();
        println!("after end: {got:?}");
        assert!(got.len() <= text.len() + 2 && got.starts_with(text), "{got:?}");
    }

    #[test]
    fn psk_realtime_factor() {
        let text = QSO;
        for n in [1usize, 16] {
            let txs: Vec<Tx> = (0..n)
                .map(|i| {
                    let mut t = Tx::new(350.0 + 165.0 * i as f64 + 1500.0 * (n == 1) as u8 as f64, 0.01, text);
                    t.start_s = 0.2 * i as f64;
                    t
                })
                .collect();
            let (mut audio, p) = audio_for(&txs, 4.0);
            add_noise(&mut audio, noise_for(p[0], 5.0), 10);
            let mut dec = PskDecoder::new(FS, 31.25);
            let r = run(&mut dec, &audio);
            let mut decoded = 0;
            for t in &txs {
                let (_, got) = text_near(&r.texts, t.hz as f32);
                if cer(text, &got) < 0.05 {
                    decoded += 1;
                } else {
                    println!("{} Hz: {got:?}", t.hz);
                }
            }
            println!(
                "{n:>2} channels: {:.1} s of audio in {:.3} s, {:.0}x realtime ({:.2}% of a core), {decoded}/{n} decoded",
                r.audio_s,
                r.secs,
                r.audio_s / r.secs,
                100.0 * r.secs / r.audio_s
            );
            assert_eq!(decoded, n);
        }
    }

    /// I/Q captures of a real band: WAV, two int16 channels (I, Q). Run with
    /// `USDR_PSK_LIVE=<dir holding psk*-iq.wav> cargo test --release
    /// psk_live -- --nocapture`; without it the test does nothing.
    fn read_iq_wav(path: &std::path::Path) -> Option<(f32, Vec<Complex32>)> {
        let b = std::fs::read(path).ok()?;
        if b.len() < 44 || &b[..4] != b"RIFF" || &b[8..12] != b"WAVE" {
            return None;
        }
        let mut at = 12;
        let mut fs = 0u32;
        while at + 8 <= b.len() {
            let id = &b[at..at + 4];
            let len = u32::from_le_bytes(b[at + 4..at + 8].try_into().ok()?) as usize;
            let body = &b[at + 8..(at + 8 + len).min(b.len())];
            if id == b"fmt " {
                fs = u32::from_le_bytes(body[4..8].try_into().ok()?);
            } else if id == b"data" {
                let iq = body
                    .chunks_exact(4)
                    .map(|c| {
                        let i = i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0;
                        let q = i16::from_le_bytes([c[2], c[3]]) as f32 / 32768.0;
                        Complex32::new(i, q)
                    })
                    .collect();
                return Some((fs as f32, iq));
            }
            at += 8 + len + (len & 1);
        }
        None
    }

    /// Noise power per sample of white noise as strong as the median of
    /// `audio`'s spectrum across 200 to 3000 Hz, which on a band of a few
    /// stations is the noise between them.
    fn noise_floor(audio: &[f32], fs: f32) -> f64 {
        let n = 4096;
        let fft = rustfft::FftPlanner::new().plan_fft_forward(n);
        let w: Vec<f32> = (0..n).map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / n as f32).cos()).collect();
        let mut acc = vec![0.0f64; n / 2];
        let mut frames = 0;
        for f in audio.chunks_exact(n) {
            let mut x: Vec<Complex32> = f.iter().zip(&w).map(|(&v, &w)| Complex32::new(v * w, 0.0)).collect();
            fft.process(&mut x);
            for (a, v) in acc.iter_mut().zip(&x) {
                *a += v.norm_sqr() as f64;
            }
            frames += 1;
        }
        let bin = fs / n as f32;
        let mut band: Vec<f64> = acc[(200.0 / bin) as usize..(3000.0 / bin) as usize].iter().map(|v| v / frames as f64).collect();
        band.sort_by(f64::total_cmp);
        let sw2: f64 = w.iter().map(|&v| (v * v) as f64).sum();
        band[band.len() / 2] / sw2
    }

    #[test]
    fn psk_live_recordings() {
        let Ok(dir) = std::env::var("USDR_PSK_LIVE") else { return };
        for name in ["psk1-iq.wav", "psk2-iq.wav", "psk3-iq.wav"] {
            let Some((fs, iq)) = read_iq_wav(&std::path::Path::new(&dir).join(name)) else {
                println!("{name}: not readable");
                continue;
            };
            // Demodulated exactly as the server does it.
            let mut ssb = crate::ssb::SsbDemod::with_passband(fs as f64, crate::ssb::Sideband::Upper, 100.0, 3100.0);
            let (mut audio, mut linear) = (Vec::new(), Vec::new());
            let mut demod = Vec::with_capacity(iq.len());
            let step = (fs * 0.02) as usize;
            for chunk in iq.chunks(step) {
                ssb.process_with_linear(chunk, &mut audio, &mut linear);
                demod.extend_from_slice(&linear);
            }
            let floor = noise_floor(&demod, fs);
            // As recorded, then with noise added to bring every station
            // down by about 6 and 10 dB.
            for down_db in [0.0, 6.0, 10.0] {
                let mut a = demod.clone();
                if down_db > 0.0 {
                    let add = floor * (10f64.powf(down_db / 10.0) - 1.0);
                    let mut rng = Rng(77 + down_db as u64);
                    a.iter_mut().for_each(|v| *v += (rng.gauss() * add.sqrt()) as f32);
                }
                let mut dec = PskDecoder::new(fs, 31.25);
                let mut texts = Vec::new();
                for chunk in a.chunks(step) {
                    dec.push(chunk);
                    texts.extend(dec.take());
                }
                // Pad with silence so the last lines time out and print.
                dec.push(&vec![0.0; (4.0 * fs) as usize]);
                texts.extend(dec.take());
                println!("== {name}, stations {down_db:.0} dB down: {:.1} s at {fs} Hz", iq.len() as f32 / fs);
                let mut ids: Vec<u32> = texts.iter().map(|t| t.channel).collect();
                ids.sort();
                ids.dedup();
                let mut all = String::new();
                for id in ids {
                    let mine: Vec<&PskText> = texts.iter().filter(|t| t.channel == id).collect();
                    let joined = mine.iter().map(|t| t.text.as_str()).collect::<Vec<_>>().join(" | ");
                    println!("  ch {id:>2} {:7.1} Hz {:5.1} dB: {joined}", mine[0].audio_hz, mine[0].snr_db);
                    all += &joined;
                    all.push('\n');
                }
                if down_db > 0.0 {
                    continue;
                }
                // What the strong station in each capture plainly sent; a
                // squelch that shut on its fades lost pieces of these.
                let want: &[&str] = match name {
                    "psk1-iq.wav" => &["Adrian Adrian", "Born ", "HAM since : 1980", "FT-897D"],
                    "psk2-iq.wav" => &["JO21QI JO21QI", "btu Robert KF3AA de PE1FTV k", "TA3/HB9FIH kn"],
                    _ => &["599 | My Nam"],
                };
                for w in want {
                    assert!(all.contains(w), "{name}: {w:?} missing");
                }
            }
        }
    }

    /// CER for a long text at `snr_db` in 2500 Hz over a channel.
    fn cer_channel(snr_db: f64, fading: Option<Fading>, seed: u64, reps: usize) -> f64 {
        let text = QSO.repeat(reps);
        let mut tx = Tx::new(1234.0, 3e-4, &text);
        tx.fading = fading;
        let (mut audio, p) = audio_for(&[tx], 4.0);
        add_noise(&mut audio, noise_for_2500(p[0], snr_db), seed);
        let mut dec = PskDecoder::new(FS, 31.25);
        let r = run(&mut dec, &audio);
        let (_, got) = text_near(&r.texts, 1234.0);
        cer(&text, &got)
    }

    /// The table for the weak-signal work: CER by SNR in 2500 Hz on a
    /// plain channel and the two CCIR fading ones. Slow; run it by name.
    #[test]
    #[ignore]
    fn psk_weak_signal_table() {
        let snrs = [0.0, -5.0, -8.0, -10.0, -12.0];
        println!("CER %      {}", snrs.map(|s| format!("{s:>7.0}")).join(""));
        for (name, fading) in [("AWGN", None), ("moderate", Some(MODERATE)), ("poor", Some(POOR))] {
            let mut row = String::new();
            for &snr in &snrs {
                // Fading wants several realizations to average over.
                let seeds: &[u64] = if fading.is_some() { &[31, 32, 33] } else { &[31] };
                let mut c = 0.0;
                for (i, &seed) in seeds.iter().enumerate() {
                    let f = fading.map(|f| Fading { seed: f.seed + 100 * i as u64, ..f });
                    c += cer_channel(snr, f, seed, 3);
                }
                row += &format!("{:>7.1}", 100.0 * c / seeds.len() as f64);
            }
            println!("{name:<10} {row}");
        }
        // Detection: how weak a signal still gets a channel, and when.
        for snr in [-14.0, -18.0, -22.0, -24.0, -26.0] {
            let tx = Tx::new(1500.0, 1e-3, QSO);
            let (mut audio, p) = audio_for(&[tx], 0.0);
            add_noise(&mut audio, noise_for_2500(p[0], snr), 41);
            let mut dec = PskDecoder::new(FS, 31.25);
            let mut found = None;
            for (i, chunk) in audio.chunks(960).enumerate() {
                dec.push(chunk);
                dec.take();
                if found.is_none() && dec.channels().iter().any(|c| (c.audio_hz - 1500.0).abs() < 5.0) {
                    found = Some((i + 1) as f64 * 0.02 - 0.5);
                }
            }
            println!("detect at {snr:>5.1} dB in 2500 Hz: {}", found.map_or("never in 60 s".to_string(), |t| format!("channel after {t:.1} s")));
        }
    }
}
