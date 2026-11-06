//! DSC: Digital Selective Calling on the MF/HF distress and safety channels
//! (2187.5, 4207.5, 6312, 8414.5, 12577 and 16804.5 kHz), per ITU-R
//! M.493-16 with the operating rules of M.541.
//!
//! **The signal.** 100 baud FSK with 170 Hz shift. With the dial 1.7 kHz
//! below the assigned frequency in upper sideband, the tones land at 1785
//! and 1615 Hz. The higher tone is the B state and the lower the Y state,
//! and M.493 Table A1-1 counts B as binary 0 and Y as 1. Ships may be 10 Hz
//! off and receivers further, so the pair is searched for within 110 Hz of
//! 1700 Hz, and only there: routine DSC channels sit 500 Hz either side of
//! the distress channels and land in the same passband.
//!
//! **Characters.** Ten bits: seven information bits, least significant
//! first, then three check bits, most significant first, that count the
//! B (zero) elements among the seven. Symbols 0 to 99 carry two decimal
//! digits each, 100 to 127 are commands (Table A1-3).
//!
//! **A call** opens with a dot pattern (200 or 20 bits of alternating B
//! and Y, which never frames as a valid character), then phasing: six DX
//! characters (125) alternating with RX characters 111, 110, … 104. Every
//! character after that is sent twice: once in a DX slot and again five
//! slots (400 ms of other characters) later in an RX slot. The format
//! specifier is sent twice in both. The call content, whose layout depends
//! on the format, category and first telecommand (Tables A1-4.1 to
//! A1-4.10), is followed by the end-of-sequence character (117, 122 or
//! 127), the error-check character, and the EOS twice more in DX slots
//! only.
//!
//! **The method.**
//!
//! - The audio is mixed from 1700 Hz to 0 Hz and decimated to about
//!   3.2 kHz, the band where the pair can be. A 512-point spectrum,
//!   averaged over about half a second, finds the pair as in the RTTY
//!   receiver (two tone windows above a median floor with a gap between
//!   them), and the last four seconds are kept and demodulated when it
//!   locks, so the dot pattern and phasing heard while it was being found
//!   are not lost.
//! - A channel filter cuts the band to ±260 Hz, rejecting the neighboring
//!   DSC channels, and each tone has a one-bit matched filter. Their
//!   levels are weighed for selective fading by the RTTY decoder's [`Atc`],
//!   which also scales the decision into a log-likelihood ratio. The bit
//!   clock follows the decision's zero crossings.
//! - Character alignment comes from character recognition, as M.493
//!   § 3.1 asks: phasing is achieved on two DX and one RX, two RX and one
//!   DX, or three RX phasing characters, each one exact, in consistent
//!   slots anywhere in the phasing sequence (§ 3.3). Both tone polarities
//!   are tried, so a call heard in lower sideband decodes too.
//! - Each character is decided from both of its copies together: their
//!   soft bits are summed and the most likely of the 128 code words taken
//!   (when both copies are valid and agree, that is the answer). A
//!   character none stands out for is unreadable and reported as such.
//!   The format specifier has four copies and the final EOS four.
//! - The call's layout is followed from the format specifier, category and
//!   first telecommand to find the EOS; the ECC is the even vertical parity
//!   of the seven information bits of one format specifier, every content
//!   character and one EOS (§ 10), and every call is reported with whether
//!   it checks. Nothing is corrected from the ECC: a call that fails it is
//!   reported as failed, not repaired into a likely answer. Seven bits of
//!   parity pass about one badly corrupted call in 128, so each field is
//!   also checked for symbols its position does not allow.
//! - A call whose signal goes before its closing sequence is reported as
//!   cut off, and the copies heard after the signal went (which the fading
//!   correction reads as confidently as signal) are not used.

use crate::rtty::Atc;
use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::f32::consts::{PI, TAU};
use std::fmt::Write as _;
use std::sync::Arc;

/// MF/HF DSC: 100 baud, 170 Hz shift, centered on 1700 Hz of audio.
const BAUD: f32 = 100.0;
const SHIFT_HZ: f32 = 170.0;
const AUDIO_CENTER_HZ: f32 = 1700.0;
/// How far from 1700 Hz the pair's center is searched for.
const SEARCH_HZ: f32 = 110.0;

/// Rate of the band around 1700 Hz, before the channel filter.
const BAND_RATE: f32 = 3200.0;
/// The front filter's passband: the pair search and its noise floor.
const BAND_PASS_HZ: f32 = 500.0;
/// The channel filter: both tones with their keying sidebands at the
/// largest offset searched, and a stopband that takes in most of a
/// neighboring DSC channel's nearer tone 500 Hz away.
const CHAN_PASS_HZ: f32 = 260.0;
const CHAN_STOP_HZ: f32 = 440.0;

/// Pair search, as in the RTTY receiver but faster: a DSC call can be as
/// short as five seconds.
const FFT_S: f32 = 0.16;
const SPEC_ALPHA: f32 = 0.2;
const MIN_FRAMES: u32 = 4;
const ACQUIRE_RHO: f32 = 0.7;
const HOLD_RHO: f32 = 0.3;
const CONFIRM: u32 = 2;
/// A pair absent this long between calls is given up.
const HOLD_S: f32 = 3.0;
/// Between calls the center follows the pair; a move larger than this is a
/// new station, and the history is demodulated again at its center.
const RECENTER_HZ: f32 = 10.0;
/// Audio kept for the replay at lock: a 200-bit dot pattern and the
/// phasing, with room for the time the search takes.
const HISTORY_S: f32 = 4.0;
/// The weaker tone may be this far below the stronger, in power.
const MAX_TILT: f32 = 0.03;
/// SNR is referred to 2500 Hz, as elsewhere in the engine.
const SNR_REF_HZ: f32 = 2500.0;
const TINY: f32 = 1e-30;

/// Bit clock loop gains, hunting and inside a call.
const CLOCK_GAIN_HUNT: f32 = 0.08;
const CLOCK_GAIN_SYNC: f32 = 0.03;
/// Bits kept for phasing detection: the 16 phasing slots and more.
const RING: usize = 256;
/// Soft bits are capped, so one very clean copy cannot drown a confident
/// disagreement from the other.
const LLR_MAX: f32 = 30.0;
/// The best code word must beat the next by this much (twice the
/// difference of their log-likelihoods) to be read.
const ML_MARGIN: f32 = 2.0;
/// A valid copy is overruled by the combined evidence only when its bits
/// are on average this much weaker than those of the copies that failed
/// their check: then it was heard in a fade and may be a wrong word that
/// happens to pass.
const VALID_OVERRIDE: f32 = 0.3;
/// The longest call: an automatic call with a nine-symbol number is 36 DX
/// characters with its closing sequence.
const MAX_DX: usize = 48;
/// A slot looks like signal when the stronger tone's power over its ten
/// bits exceeds the weaker's by this ratio. Noise alone gives about 2; a
/// call at -3 dB in 2500 Hz has a median near 7 and at -6 dB near 4, so
/// one slot proves little, but a stretch of them does: in noise about one
/// slot in six passes, in a call at -6 dB two in three.
const GOOD_Q: f32 = 3.2;
/// The signal has gone when no more than this share of the slots since
/// some point have looked like signal.
const GONE_SHARE: f32 = 0.2;
/// A call ends when the last this many slots say the signal has gone.
const LOST_SLOTS: usize = 16;
/// A call that did not close is taken to have been cut off where the
/// last stretch of at least this many slots that say so began.
const CUT_SLOTS: usize = 3;

/// Format specifiers (§ 4).
pub const FMT_AREA: u8 = 102;
pub const FMT_DISTRESS: u8 = 112;
pub const FMT_GROUP: u8 = 114;
pub const FMT_ALL_SHIPS: u8 = 116;
pub const FMT_INDIVIDUAL: u8 = 120;
pub const FMT_AUTOMATIC: u8 = 123;
/// Categories (§ 6).
pub const CAT_DISTRESS: u8 = 112;
/// First telecommands that turn a call into a distress acknowledgment or
/// a distress alert relay.
pub const TC_DISTRESS_ACK: u8 = 110;
pub const TC_DISTRESS_RELAY: u8 = 112;
pub const TC_POSITION: u8 = 121;
/// End of sequence (§ 9).
pub const EOS_ACK_RQ: u8 = 117;
pub const EOS_ACK_BQ: u8 = 122;
pub const EOS_OTHER: u8 = 127;
/// Sent in place of unused information.
pub const NO_INFO: u8 = 126;
const DX_PHASING: u8 = 125;
/// Message 2 of a call carrying the ship's position starts with this.
const POSITION_MARK: u8 = 55;

fn is_eos(v: u8) -> bool {
    matches!(v, EOS_ACK_RQ | EOS_ACK_BQ | EOS_OTHER)
}

fn known_format(v: u8) -> bool {
    matches!(v, FMT_AREA | FMT_DISTRESS | FMT_GROUP | FMT_ALL_SHIPS | FMT_INDIVIDUAL | FMT_AUTOMATIC)
}

// ---------------------------------------------------------------------------
// The code.

/// The ten-bit code word for a symbol: bit j is the j-th bit sent, 1 for Y.
const fn codeword(v: u8) -> u16 {
    let info = (v & 0x7f) as u16;
    let b = 7 - (v & 0x7f).count_ones() as u16;
    info | ((b >> 2) & 1) << 7 | ((b >> 1) & 1) << 8 | (b & 1) << 9
}

const fn codewords() -> [u16; 128] {
    let mut t = [0u16; 128];
    let mut v = 0;
    while v < 128 {
        t[v] = codeword(v as u8);
        v += 1;
    }
    t
}

const CODEWORDS: [u16; 128] = codewords();

/// The symbol a hard word is, if it is a valid code word.
fn decode_hard(w: u16) -> Option<u8> {
    let v = (w & 0x7f) as u8;
    (CODEWORDS[v as usize] == w).then_some(v)
}

/// One received character: soft bits as log-likelihood ratios in the order
/// sent, positive for Y (binary 1).
type Soft = [f32; 10];

fn hard(s: &Soft) -> u16 {
    s.iter().enumerate().fold(0, |w, (j, &x)| w | (u16::from(x > 0.0) << j))
}

/// A character from any number of copies: the most likely code word given
/// all their soft bits, or None when none stands out by [`ML_MARGIN`].
///
/// Summing the copies' log-likelihood ratios gives the evidence of them
/// all together, so this covers the classic rules (a valid copy against an
/// invalid one, two copies with an error each) and the cases they cannot:
/// two invalid copies, and a valid but weak copy outvoted by a confident
/// one.
fn combine(copies: &[&Soft]) -> Option<u8> {
    // Copies that are valid code words and agree, the others failing their
    // check: the classic rule takes them, since a copy that fails its
    // check is known to be wrong somewhere. Summing alone would not when
    // the failed copy is the more confident one, as a burst of static
    // makes it: its wrong bits would outvote the clean copy.
    let mut valid = None;
    let mut agree = true;
    let (mut conf_valid, mut conf_failed) = (f32::MAX, 0.0f32);
    for c in copies {
        let conf = c.iter().map(|x| x.abs()).sum::<f32>() / 10.0;
        if let Some(v) = decode_hard(hard(c)) {
            agree &= valid.is_none_or(|u| u == v);
            valid = Some(v);
            conf_valid = conf_valid.min(conf);
        } else {
            conf_failed = conf_failed.max(conf);
        }
    }
    let valid = valid.filter(|_| agree);
    if valid.is_some() && copies.iter().all(|c| decode_hard(hard(c)) == valid) {
        return valid;
    }
    if valid.is_some() && conf_valid >= VALID_OVERRIDE * conf_failed {
        return valid;
    }
    let mut sum = [0.0f32; 10];
    for c in copies {
        for (s, x) in sum.iter_mut().zip(c.iter()) {
            *s += x;
        }
    }
    let (mut best, mut best_m, mut second_m) = (0u8, f32::MIN, f32::MIN);
    for (v, &w) in CODEWORDS.iter().enumerate() {
        let m: f32 = sum.iter().enumerate().map(|(j, s)| if w >> j & 1 == 1 { *s } else { -*s }).sum();
        if m > best_m {
            second_m = best_m;
            best_m = m;
            best = v as u8;
        } else if m > second_m {
            second_m = m;
        }
    }
    (best_m - second_m >= ML_MARGIN).then_some(best)
}

// ---------------------------------------------------------------------------
// Small DSP pieces.

/// Blackman-windowed sinc lowpass, unity gain at DC, with its transition
/// from `pass` to `stop` Hz.
fn lowpass(fs: f32, pass: f32, stop: f32, max_taps: usize) -> Vec<f32> {
    let half = (0.5 * (stop - pass)).max(1.0);
    let n = ((2.75 * fs / half).ceil() as usize).clamp(15, max_taps) | 1;
    let cutoff = 0.5 * (pass + stop) / fs;
    let mid = (n / 2) as f32;
    let m = (n - 1) as f32;
    let mut taps: Vec<f32> = (0..n)
        .map(|i| {
            let x = i as f32 - mid;
            let sinc = if x == 0.0 { 2.0 * cutoff } else { (TAU * cutoff * x).sin() / (PI * x) };
            sinc * (0.42 - 0.5 * (TAU * i as f32 / m).cos() + 0.08 * (2.0 * TAU * i as f32 / m).cos())
        })
        .collect();
    let sum: f32 = taps.iter().sum();
    taps.iter_mut().for_each(|t| *t /= sum);
    taps
}

/// A complex phasor stepped by multiplication, renormalized now and then.
#[derive(Clone, Copy)]
struct Osc {
    cur: Complex32,
    step: Complex32,
    n: u32,
}

impl Osc {
    /// Steps by `-hz` at `fs`: mixing with it moves `hz` to 0.
    fn new(hz: f64, fs: f64) -> Self {
        let mut o = Self { cur: Complex32::new(1.0, 0.0), step: Complex32::new(1.0, 0.0), n: 0 };
        o.set(hz, fs);
        o
    }

    fn set(&mut self, hz: f64, fs: f64) {
        let w = -std::f64::consts::TAU * hz / fs;
        self.step = Complex32::new(w.cos() as f32, w.sin() as f32);
    }

    #[inline]
    fn next(&mut self) -> Complex32 {
        let v = self.cur;
        self.cur *= self.step;
        self.n += 1;
        if self.n >= 512 {
            self.n = 0;
            self.cur *= 1.5 - 0.5 * self.cur.norm_sqr();
        }
        v
    }
}

/// Real audio in, the band around 1700 Hz out at about 3.2 kHz, complex.
///
/// Mixing then lowpass filtering is the same as filtering with the lowpass
/// moved up to 1700 Hz and mixing only the samples kept, which is what this
/// does: the filter runs on the real input, one output in `decim`.
struct FrontEnd {
    taps_re: Vec<f32>,
    taps_im: Vec<f32>,
    buf: Vec<f32>,
    pos: usize,
    decim: usize,
    phase: usize,
    rot: Osc,
}

impl FrontEnd {
    fn new(fs: f32, decim: usize) -> Self {
        let fs2 = fs / decim as f32;
        let h = lowpass(fs, BAND_PASS_HZ, (fs2 - BAND_PASS_HZ).max(BAND_PASS_HZ + 100.0), 511);
        let n = h.len();
        let w = std::f64::consts::TAU * AUDIO_CENTER_HZ as f64 / fs as f64;
        // The window holds the oldest sample first: entry j is k = n-1-j
        // samples old, and carries the lowpass tap times e^{jwk}.
        let (mut taps_re, mut taps_im) = (Vec::with_capacity(n), Vec::with_capacity(n));
        for j in 0..n {
            let k = (n - 1 - j) as f64;
            taps_re.push(h[n - 1 - j] * (w * k).cos() as f32);
            taps_im.push(h[n - 1 - j] * (w * k).sin() as f32);
        }
        Self {
            taps_re,
            taps_im,
            buf: vec![0.0; 2 * n],
            pos: 0,
            decim,
            phase: 0,
            rot: Osc::new(AUDIO_CENTER_HZ as f64 * decim as f64, fs as f64),
        }
    }

    #[inline]
    fn push(&mut self, x: f32) -> Option<Complex32> {
        let n = self.taps_re.len();
        self.buf[self.pos] = x;
        self.buf[self.pos + n] = x;
        self.pos += 1;
        if self.pos == n {
            self.pos = 0;
        }
        self.phase += 1;
        if self.phase < self.decim {
            return None;
        }
        self.phase = 0;
        let win = &self.buf[self.pos..self.pos + n];
        let (mut re, mut im) = ([0.0f32; 4], [0.0f32; 4]);
        let mut wc = win.chunks_exact(4);
        let mut rc = self.taps_re.chunks_exact(4);
        let mut ic = self.taps_im.chunks_exact(4);
        for ((w, r), i) in (&mut wc).zip(&mut rc).zip(&mut ic) {
            for k in 0..4 {
                re[k] += r[k] * w[k];
                im[k] += i[k] * w[k];
            }
        }
        let (mut sr, mut si) = (re.iter().sum::<f32>(), im.iter().sum::<f32>());
        for ((w, r), i) in wc.remainder().iter().zip(rc.remainder()).zip(ic.remainder()) {
            sr += r * w;
            si += i * w;
        }
        Some(Complex32::new(sr, si) * self.rot.next())
    }
}

/// Complex lowpass with real taps, decimating by two.
struct Decim2 {
    taps: Vec<f32>,
    buf: Vec<Complex32>,
    pos: usize,
    phase: bool,
}

impl Decim2 {
    fn new(taps: Vec<f32>) -> Self {
        let n = taps.len();
        Self { taps, buf: vec![Complex32::new(0.0, 0.0); 2 * n], pos: 0, phase: false }
    }

    fn reset(&mut self) {
        self.buf.fill(Complex32::new(0.0, 0.0));
        self.pos = 0;
        self.phase = false;
    }

    #[inline]
    fn push(&mut self, x: Complex32) -> Option<Complex32> {
        let n = self.taps.len();
        self.buf[self.pos] = x;
        self.buf[self.pos + n] = x;
        self.pos += 1;
        if self.pos == n {
            self.pos = 0;
        }
        self.phase = !self.phase;
        if self.phase {
            return None;
        }
        let win = &self.buf[self.pos..self.pos + n];
        let (mut re, mut im) = (0.0f32, 0.0f32);
        for (t, w) in self.taps.iter().zip(win) {
            re += t * w.re;
            im += t * w.im;
        }
        Some(Complex32::new(re, im))
    }
}

/// The channel filter and each tone's one-bit matched filter: the
/// amplitude of each tone over the last bit.
struct Demod {
    fs3: f32,
    chan: Decim2,
    osc_hi: Osc,
    osc_lo: Osc,
    ring_hi: Vec<Complex32>,
    ring_lo: Vec<Complex32>,
    rpos: usize,
    sum_hi: Complex32,
    sum_lo: Complex32,
    since_resum: u32,
}

impl Demod {
    fn new(fs2: f32) -> Self {
        let fs3 = 0.5 * fs2;
        let len = ((fs3 / BAUD).round() as usize).max(2);
        Self {
            fs3,
            chan: Decim2::new(lowpass(fs2, CHAN_PASS_HZ, CHAN_STOP_HZ, 255)),
            osc_hi: Osc::new(0.5 * SHIFT_HZ as f64, fs3 as f64),
            osc_lo: Osc::new(-0.5 * SHIFT_HZ as f64, fs3 as f64),
            ring_hi: vec![Complex32::new(0.0, 0.0); len],
            ring_lo: vec![Complex32::new(0.0, 0.0); len],
            rpos: 0,
            sum_hi: Complex32::new(0.0, 0.0),
            sum_lo: Complex32::new(0.0, 0.0),
            since_resum: 0,
        }
    }

    /// The pair's center, as an offset from 1700 Hz, keeping the filters'
    /// state.
    fn set_center(&mut self, c: f32) {
        self.osc_hi.set((c + 0.5 * SHIFT_HZ) as f64, self.fs3 as f64);
        self.osc_lo.set((c - 0.5 * SHIFT_HZ) as f64, self.fs3 as f64);
    }

    fn reset(&mut self, c: f32) {
        self.set_center(c);
        self.chan.reset();
        self.ring_hi.fill(Complex32::new(0.0, 0.0));
        self.ring_lo.fill(Complex32::new(0.0, 0.0));
        self.rpos = 0;
        self.sum_hi = Complex32::new(0.0, 0.0);
        self.sum_lo = Complex32::new(0.0, 0.0);
        self.since_resum = 0;
    }

    #[inline]
    fn push(&mut self, z: Complex32) -> Option<(f32, f32)> {
        let y = self.chan.push(z)?;
        let h = y * self.osc_hi.next();
        let l = y * self.osc_lo.next();
        let old_h = std::mem::replace(&mut self.ring_hi[self.rpos], h);
        let old_l = std::mem::replace(&mut self.ring_lo[self.rpos], l);
        self.rpos += 1;
        if self.rpos == self.ring_hi.len() {
            self.rpos = 0;
        }
        self.sum_hi += h - old_h;
        self.sum_lo += l - old_l;
        self.since_resum += 1;
        if self.since_resum >= 4096 {
            // Rounding in the add-and-subtract must not build up.
            self.since_resum = 0;
            self.sum_hi = self.ring_hi.iter().sum();
            self.sum_lo = self.ring_lo.iter().sum();
        }
        let inv = 1.0 / self.ring_hi.len() as f32;
        Some((self.sum_hi.norm() * inv, self.sum_lo.norm() * inv))
    }
}

/// Excess of the two tone windows and the gap between them over the floor.
#[derive(Clone, Copy, Debug, Default)]
struct Pair {
    rho_hi: f32,
    rho_lo: f32,
    rho_mid: f32,
    power: f32,
}

impl Pair {
    /// The weaker tone's excess, or zero when this is not shaped like a
    /// pair: a lone carrier between the tones fills the gap, and one whose
    /// skirt reaches a window leaves the other far weaker.
    fn score(&self) -> f32 {
        let weak = self.rho_hi.min(self.rho_lo);
        let strong = self.rho_hi.max(self.rho_lo);
        if weak < MAX_TILT * strong || self.rho_mid > weak {
            return 0.0;
        }
        weak
    }
}

/// The averaged spectrum of the band, and the pair search over it.
struct Spectrum {
    n: usize,
    hop: usize,
    bin_hz: f32,
    fft: Arc<dyn Fft<f32>>,
    win: Vec<f32>,
    ring: Vec<Complex32>,
    pos: usize,
    filled: usize,
    since: usize,
    work: Vec<Complex32>,
    scratch: Vec<Complex32>,
    avg: Vec<f32>,
    frames: u32,
    sorted: Vec<f32>,
    floor: f32,
}

impl Spectrum {
    fn new(fs2: f32) -> Self {
        let n = ((fs2 * FFT_S) as usize).next_power_of_two().max(64);
        let fft = FftPlanner::new().plan_fft_forward(n);
        let scratch = vec![Complex32::new(0.0, 0.0); fft.get_inplace_scratch_len()];
        Self {
            n,
            hop: n / 2,
            bin_hz: fs2 / n as f32,
            fft,
            win: (0..n).map(|i| 0.5 - 0.5 * (TAU * i as f32 / n as f32).cos()).collect(),
            ring: vec![Complex32::new(0.0, 0.0); n],
            pos: 0,
            filled: 0,
            since: 0,
            work: vec![Complex32::new(0.0, 0.0); n],
            scratch,
            avg: vec![0.0; n],
            frames: 0,
            sorted: Vec::with_capacity(n),
            floor: 0.0,
        }
    }

    /// One sample in; true when a new averaged spectrum is ready.
    fn push(&mut self, z: Complex32) -> bool {
        self.ring[self.pos] = z;
        self.pos = (self.pos + 1) % self.n;
        self.filled = (self.filled + 1).min(self.n);
        self.since += 1;
        if self.since < self.hop || self.filled < self.n {
            return false;
        }
        self.since = 0;
        let n = self.n;
        for (i, w) in self.work.iter_mut().enumerate() {
            *w = self.ring[(self.pos + i) % n] * self.win[i];
        }
        self.fft.process_with_scratch(&mut self.work, &mut self.scratch);
        self.frames += 1;
        let a = (1.0 / self.frames as f32).max(SPEC_ALPHA);
        for (avg, w) in self.avg.iter_mut().zip(&self.work) {
            *avg += a * (w.norm_sqr() - *avg);
        }
        // The median of the passband is the floor: the pair takes a small
        // share of it.
        let reach = (BAND_PASS_HZ / self.bin_hz) as i32;
        self.sorted.clear();
        for k in -reach..=reach {
            self.sorted.push(self.avg[self.bin(k)]);
        }
        let mid = self.sorted.len() / 2;
        let (_, m, _) = self.sorted.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
        self.floor = *m;
        true
    }

    fn bin(&self, k: i32) -> usize {
        k.rem_euclid(self.n as i32) as usize
    }

    /// Bins from `a` to `b` Hz, inclusive of the nearest.
    fn range(&self, a: f32, b: f32) -> std::ops::RangeInclusive<i32> {
        (a / self.bin_hz).round() as i32..=(b / self.bin_hz).round() as i32
    }

    fn excess(&self, a: f32, b: f32) -> (f32, f32) {
        let r = self.range(a, b);
        let n = (r.end() - r.start() + 1) as f32;
        debug_assert!(n > 0.0);
        let e: f32 = r.map(|k| self.avg[self.bin(k)]).sum::<f32>() - n * self.floor;
        (e / (n * self.floor), e)
    }

    /// The pair centered `c` Hz from 1700. The tone windows take in a
    /// tone's keying sidebands but stop short of the other tone's, as in
    /// the RTTY receiver.
    fn pair_at(&self, c: f32) -> Pair {
        if self.floor <= 0.0 || !self.floor.is_finite() {
            return Pair::default();
        }
        let hs = 0.5 * SHIFT_HZ;
        let hw = (0.75 * BAUD).min(0.4 * SHIFT_HZ);
        let (rho_hi, p_hi) = self.excess(c + hs - hw, c + hs + hw);
        let (rho_lo, p_lo) = self.excess(c - hs - hw, c - hs + hw);
        let gap = (hs - hw).max(0.5 * self.bin_hz);
        let (rho_mid, _) = self.excess(c - gap, c + gap);
        Pair { rho_hi, rho_lo, rho_mid, power: p_hi.max(0.0) + p_lo.max(0.0) }
    }

    /// Noise-subtracted centroid of `center` ± `half` Hz. The bins at the
    /// edges count for the share of them inside, so the window is not
    /// pulled to the bin grid.
    fn centroid(&self, center: f32, half: f32) -> Option<f32> {
        let (mut sw, mut swf) = (0.0f32, 0.0f32);
        for k in self.range(center - half - self.bin_hz, center + half + self.bin_hz) {
            let f = k as f32 * self.bin_hz;
            let share = ((half - (f - center).abs()) / self.bin_hz + 0.5).clamp(0.0, 1.0);
            let p = share * (self.avg[self.bin(k)] - self.floor).max(0.0);
            sw += p;
            swf += p * f;
        }
        (sw > 0.0).then(|| swf / sw)
    }

    /// The best pair within reach of 1700 Hz: its center offset and score.
    fn best(&self) -> Option<(f32, f32)> {
        if self.floor <= 0.0 || !self.floor.is_finite() {
            return None;
        }
        let reach = (SEARCH_HZ / self.bin_hz).round() as i32;
        let (cb, score) = (-reach..=reach)
            .map(|k| (k, self.pair_at(k as f32 * self.bin_hz).score()))
            .max_by(|a, b| a.1.total_cmp(&b.1))?;
        // Each tone's centroid, and the center halfway between: DSC code
        // words are not balanced (phasing is mostly Y), so the centroid of
        // the whole pair leans toward the lower tone, while each tone's
        // shape is the other's mirror image and their biases cancel. Three
        // times, so the windows settle on the tones rather than on the
        // bin the search found.
        let (hs, hw) = (0.5 * SHIFT_HZ, (0.75 * BAUD).min(0.4 * SHIFT_HZ));
        let mut c = cb as f32 * self.bin_hz;
        for _ in 0..3 {
            match (self.centroid(c + hs, hw), self.centroid(c - hs, hw)) {
                (Some(h), Some(l)) => c = 0.5 * (h + l),
                _ => break,
            }
        }
        Some((c.clamp(-SEARCH_HZ, SEARCH_HZ), score))
    }

    fn snr_db(&self, p: &Pair) -> f32 {
        let noise = self.floor * SNR_REF_HZ / self.bin_hz;
        10.0 * (p.power.max(TINY) / noise.max(TINY)).log10()
    }
}

// ---------------------------------------------------------------------------
// What a call says.

/// A maritime mobile service identity (ITU-R M.585), as sent: nine digits
/// and a tenth that is 0 unless the equipment follows M.1080.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mmsi {
    pub digits: String,
    pub digit10: u8,
}

/// What an MMSI's leading digits say the station is (M.585).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MmsiKind {
    Ship,
    /// 00 and a MID.
    Coast,
    /// 0 and a MID.
    Group,
    /// 111 and a MID.
    SarAircraft,
    /// 970: AIS search and rescue transmitter.
    Sart,
    /// 972: man overboard device.
    Mob,
    /// 974: EPIRB with AIS.
    Epirb,
    /// 8 and a MID.
    Handheld,
    /// 98 and a MID: craft associated with a parent ship.
    Craft,
    /// 99 and a MID.
    AidToNavigation,
    Other,
}

impl MmsiKind {
    pub fn name(self) -> &'static str {
        match self {
            MmsiKind::Ship => "ship",
            MmsiKind::Coast => "coast",
            MmsiKind::Group => "group",
            MmsiKind::SarAircraft => "sarAircraft",
            MmsiKind::Sart => "sart",
            MmsiKind::Mob => "mob",
            MmsiKind::Epirb => "epirb",
            MmsiKind::Handheld => "handheld",
            MmsiKind::Craft => "craft",
            MmsiKind::AidToNavigation => "aidToNavigation",
            MmsiKind::Other => "other",
        }
    }
}

impl Mmsi {
    fn from_symbols(s: &[Option<u8>]) -> Option<Mmsi> {
        let mut d = String::with_capacity(10);
        for v in s {
            let v = v.filter(|v| *v < 100)?;
            let _ = write!(d, "{v:02}");
        }
        (d.len() == 10).then(|| Mmsi { digit10: d.as_bytes()[9] - b'0', digits: d[..9].to_string() })
    }

    pub fn kind(&self) -> MmsiKind {
        let d = self.digits.as_str();
        if d.starts_with("00") {
            MmsiKind::Coast
        } else if d.starts_with('0') {
            MmsiKind::Group
        } else if d.starts_with("111") {
            MmsiKind::SarAircraft
        } else if d.starts_with("970") {
            MmsiKind::Sart
        } else if d.starts_with("972") {
            MmsiKind::Mob
        } else if d.starts_with("974") {
            MmsiKind::Epirb
        } else if d.starts_with("98") {
            MmsiKind::Craft
        } else if d.starts_with("99") {
            MmsiKind::AidToNavigation
        } else if d.starts_with('8') {
            MmsiKind::Handheld
        } else if matches!(d.as_bytes()[0], b'2'..=b'7') {
            MmsiKind::Ship
        } else {
            MmsiKind::Other
        }
    }

    /// The maritime identification digits (the country), where the kind of
    /// identity carries them and they are in the allocated range 2xx-7xx.
    pub fn mid(&self) -> Option<u16> {
        let at = match self.kind() {
            MmsiKind::Ship => 0,
            MmsiKind::Group | MmsiKind::Handheld => 1,
            MmsiKind::Coast | MmsiKind::Craft | MmsiKind::AidToNavigation => 2,
            MmsiKind::SarAircraft => 3,
            MmsiKind::Sart | MmsiKind::Mob | MmsiKind::Epirb | MmsiKind::Other => return None,
        };
        let m: u16 = self.digits[at..at + 3].parse().ok()?;
        (200..800).contains(&m).then_some(m)
    }
}

/// A position: quadrant digit, latitude and longitude in degrees and
/// minutes (§ 8.1.2).
#[derive(Clone, Debug, PartialEq)]
pub enum Position {
    Known { north: bool, east: bool, lat_deg: u8, lat_min: u8, lon_deg: u16, lon_min: u8 },
    /// Ten nines: the sender had no position, or one over 23.5 hours old.
    Unavailable,
    /// Digits outside the ranges a position can have, as sent.
    Invalid(String),
}

impl Position {
    fn from_symbols(s: &[Option<u8>]) -> Option<Position> {
        let d = digits(s)?;
        if d.iter().all(|&x| x == 9) {
            return Some(Position::Unavailable);
        }
        let q = d[0];
        let lat_deg = 10 * d[1] + d[2];
        let lat_min = 10 * d[3] + d[4];
        let lon_deg = 100 * d[5] as u16 + 10 * d[6] as u16 + d[7] as u16;
        let lon_min = 10 * d[8] + d[9];
        if q > 3 || lat_deg > 90 || lat_min > 59 || lon_deg > 180 || lon_min > 59 {
            return Some(Position::Invalid(d.iter().map(|x| char::from(b'0' + x)).collect()));
        }
        Some(Position::Known { north: q < 2, east: q % 2 == 0, lat_deg, lat_min, lon_deg, lon_min })
    }

    /// Decimal degrees, north and east positive.
    pub fn lat_lon(&self) -> Option<(f64, f64)> {
        match *self {
            Position::Known { north, east, lat_deg, lat_min, lon_deg, lon_min } => {
                let lat = lat_deg as f64 + lat_min as f64 / 60.0;
                let lon = lon_deg as f64 + lon_min as f64 / 60.0;
                Some((if north { lat } else { -lat }, if east { lon } else { -lon }))
            }
            _ => None,
        }
    }

    pub fn text(&self) -> String {
        match self {
            Position::Known { north, east, lat_deg, lat_min, lon_deg, lon_min } => format!(
                "{lat_deg:02}-{lat_min:02}{} {lon_deg:03}-{lon_min:02}{}",
                if *north { 'N' } else { 'S' },
                if *east { 'E' } else { 'W' }
            ),
            Position::Unavailable => "unavailable".into(),
            Position::Invalid(d) => format!("invalid ({d})"),
        }
    }
}

/// The ten digits of five symbols, or None if any is unreadable or not a
/// digit pair.
fn digits(s: &[Option<u8>]) -> Option<Vec<u8>> {
    let mut d = Vec::with_capacity(2 * s.len());
    for v in s {
        let v = v.filter(|v| *v < 100)?;
        d.push(v / 10);
        d.push(v % 10);
    }
    Some(d)
}

/// A time of day, UTC (§ 8.1.3).
#[derive(Clone, Debug, PartialEq)]
pub enum Utc {
    Known(u8, u8),
    /// 8888: the sender had no time.
    Unavailable,
    Invalid(String),
}

impl Utc {
    fn from_symbols(s: &[Option<u8>]) -> Option<Utc> {
        let (h, m) = (s.first().copied().flatten()?, s.get(1).copied().flatten()?);
        if h >= 100 || m >= 100 {
            return None;
        }
        Some(match (h, m) {
            (88, 88) => Utc::Unavailable,
            (h, m) if h < 24 && m < 60 => Utc::Known(h, m),
            (h, m) => Utc::Invalid(format!("{h:02}{m:02}")),
        })
    }

    pub fn text(&self) -> String {
        match self {
            Utc::Known(h, m) => format!("{h:02}:{m:02} UTC"),
            Utc::Unavailable => "unavailable".into(),
            Utc::Invalid(d) => format!("invalid ({d})"),
        }
    }
}

/// A geographic area address (§ 5.3): the rectangle's north-west corner
/// and its sides, in degrees.
#[derive(Clone, Debug, PartialEq)]
pub struct Area {
    pub north: bool,
    pub east: bool,
    pub lat_deg: u8,
    pub lon_deg: u16,
    /// North-to-south side.
    pub dlat: u8,
    /// West-to-east side.
    pub dlon: u8,
}

impl Area {
    fn from_symbols(s: &[Option<u8>]) -> Option<Area> {
        let d = digits(s)?;
        let lat_deg = 10 * d[1] + d[2];
        let lon_deg = 100 * d[3] as u16 + 10 * d[4] as u16 + d[5] as u16;
        if d[0] > 3 || lat_deg > 90 || lon_deg > 180 {
            return None;
        }
        Some(Area { north: d[0] < 2, east: d[0] % 2 == 0, lat_deg, lon_deg, dlat: 10 * d[6] + d[7], dlon: 10 * d[8] + d[9] })
    }

    pub fn text(&self) -> String {
        format!(
            "{:02}{} {:03}{} {}x{} deg",
            self.lat_deg,
            if self.north { 'N' } else { 'S' },
            self.lon_deg,
            if self.east { 'E' } else { 'W' },
            self.dlat,
            self.dlon
        )
    }
}

/// Who a call is for.
#[derive(Clone, Debug, PartialEq)]
pub enum Address {
    /// Distress alerts and all-ships calls carry no address.
    AllStations,
    Station(Mmsi),
    Area(Area),
}

/// A channel or frequency message element (§ 8.3.2, Table A1-5).
#[derive(Clone, Debug, PartialEq)]
pub enum Freq {
    /// A frequency in Hz, sent in 100 Hz or (seven digits) 10 Hz steps.
    Hz(u32),
    /// An HF/MF working channel number (leading digit 3).
    HfChannel(u32),
    /// A VHF channel; `simplex` is the M digit (1: ship frequency, 2: coast
    /// frequency, used simplex).
    VhfChannel { channel: u16, simplex: u8 },
    /// Any other leading digit (8 is M.586 equipment): the digits as sent.
    Other(String),
}

impl Freq {
    pub fn text(&self) -> String {
        match self {
            Freq::Hz(hz) if hz % 100 == 0 => format!("{:.1} kHz", *hz as f64 / 1e3),
            Freq::Hz(hz) => format!("{:.2} kHz", *hz as f64 / 1e3),
            Freq::HfChannel(c) => format!("HF/MF channel {c}"),
            Freq::VhfChannel { channel, simplex: 0 } => format!("VHF channel {channel}"),
            Freq::VhfChannel { channel, simplex } => format!("VHF channel {channel} (simplex {simplex})"),
            Freq::Other(d) => format!("code {d}"),
        }
    }
}

/// One frequency element at the start of `m`: what it says (None for "no
/// information") and how many symbols it took, or None when it cannot be
/// read.
fn freq_element(m: &[Option<u8>], no_info_len: usize) -> Option<(Option<Freq>, usize)> {
    let first = m.first().copied().flatten()?;
    if first == NO_INFO {
        let n = m.iter().take(no_info_len).take_while(|v| **v == Some(NO_INFO)).count();
        return Some((None, n));
    }
    if first >= 100 {
        return None;
    }
    let hm = first / 10;
    let n = if hm == 4 { 4 } else { 3 };
    let d = digits(m.get(..n)?)?;
    let num = |ds: &[u8]| ds.iter().fold(0u32, |a, &x| 10 * a + x as u32);
    let f = match hm {
        0..=2 => Freq::Hz(num(&d) * 100),
        3 => Freq::HfChannel(num(&d[1..])),
        4 => Freq::Hz(num(&d[1..]) * 10),
        9 => Freq::VhfChannel { channel: num(&d[3..]) as u16, simplex: d[2] },
        _ => Freq::Other(d.iter().map(|x| char::from(b'0' + x)).collect()),
    };
    Some((Some(f), n))
}

/// The distress information of alerts, acknowledgments and relays
/// (§ 8.1, § 8.2).
#[derive(Clone, Debug, PartialEq)]
pub struct Distress {
    /// The station in distress. For an alert, the sender itself.
    pub id: Option<DistressId>,
    pub nature: Option<u8>,
    pub position: Option<Position>,
    pub time: Option<Utc>,
    /// The mode of subsequent communication (a first telecommand).
    pub subsequent: Option<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DistressId {
    Known(Mmsi),
    /// Five 126s: relayed on behalf of a station whose identity is not known.
    Unknown,
}

/// One received call.
#[derive(Clone, Debug, PartialEq)]
pub struct DscCall {
    pub format: u8,
    /// None for a distress alert, which has none (the format implies it).
    pub category: Option<u8>,
    pub to: Option<Address>,
    pub from: Option<Mmsi>,
    pub tc1: Option<u8>,
    pub tc2: Option<u8>,
    pub distress: Option<Distress>,
    /// The called station's receive and transmit frequencies.
    pub rx_freq: Option<Freq>,
    pub tx_freq: Option<Freq>,
    /// A ship's position in a position call or reply, and its time.
    pub position: Option<Position>,
    pub time: Option<Utc>,
    /// The public network number of an automatic call.
    pub number: Option<String>,
    /// Message 2 and 3 symbols as received, unreadable ones None.
    pub message: Vec<Option<u8>>,
    pub eos: Option<u8>,
    /// The error-check character matched the call.
    pub ecc_ok: bool,
    /// Fields holding a symbol their position does not allow (a command
    /// where digits belong, an unassigned command, a position or time out
    /// of range). M.493 asks for such calls to be rejected; they are
    /// reported with this list instead. The ECC is only seven bits of
    /// parity, so this also catches some of the corrupted calls that pass
    /// it by chance.
    pub bad_fields: Vec<&'static str>,
    /// The closing sequence was received; false for a call cut off.
    pub complete: bool,
    /// Characters the DX copy alone did not give and the RX copy decided.
    pub rx_used: u32,
    /// Characters neither copy gave.
    pub unreadable: u32,
    /// Every character of the call from the format specifier to the ECC.
    pub symbols: Vec<Option<u8>>,
    pub center_hz: f32,
    pub snr_db: Option<f32>,
    /// Heard with the tones swapped (lower sideband).
    pub reverse: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DscEvent {
    Call(DscCall),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DscStatus {
    /// Locked on a tone pair near 1700 Hz that is still there.
    pub locked: bool,
    /// The pair's center in the audio.
    pub center_hz: Option<f32>,
    pub snr_db: Option<f32>,
    /// Phasing was found and a call is being received.
    pub receiving: bool,
    /// DX characters of the call being received, or of the last one.
    pub symbols: u32,
    pub reverse: bool,
    /// Calls reported since the decoder started.
    pub calls: u32,
}

pub fn format_name(v: u8) -> &'static str {
    match v {
        FMT_AREA => "Geographic area",
        FMT_DISTRESS => "Distress",
        FMT_GROUP => "Group",
        FMT_ALL_SHIPS => "All ships",
        FMT_INDIVIDUAL => "Individual",
        FMT_AUTOMATIC => "Individual automatic",
        _ => "Unknown",
    }
}

pub fn category_name(v: u8) -> &'static str {
    match v {
        100 => "Routine",
        106 => "ACS",
        108 => "Safety",
        110 => "Urgency",
        112 => "Distress",
        _ => "Unknown",
    }
}

pub fn nature_name(v: u8) -> &'static str {
    match v {
        100 => "Fire, explosion",
        101 => "Flooding",
        102 => "Collision",
        103 => "Grounding",
        104 => "Listing, in danger of capsizing",
        105 => "Sinking",
        106 => "Disabled and adrift",
        107 => "Undesignated distress",
        108 => "Abandoning ship",
        109 => "Piracy/armed robbery attack",
        110 => "Man overboard",
        112 => "EPIRB emission",
        NO_INFO => "No information",
        _ => "Unknown",
    }
}

pub fn tc1_name(v: u8) -> &'static str {
    match v {
        100 => "F3E/G3E all modes telephony",
        101 => "F3E/G3E duplex telephony",
        103 => "Polling",
        104 => "Unable to comply",
        105 => "End of call",
        106 => "Data",
        109 => "J3E telephony",
        110 => "Distress acknowledgment",
        112 => "Distress alert relay",
        113 => "F1B/J2B TTY-FEC",
        115 => "F1B/J2B TTY-ARQ",
        118 => "Test",
        121 => "Ship position or location registration updating",
        NO_INFO => "No information",
        _ => "Unknown",
    }
}

pub fn tc2_name(v: u8) -> &'static str {
    match v {
        100 => "No reason given",
        101 => "Congestion at maritime switching center",
        102 => "Busy",
        103 => "Queue indication",
        104 => "Station barred",
        105 => "No operator available",
        106 => "Operator temporarily unavailable",
        107 => "Equipment disabled",
        108 => "Unable to use proposed channel",
        109 => "Unable to use proposed mode",
        110 => "Ships and aircraft of States not parties to an armed conflict",
        111 => "Medical transports",
        112 => "Pay-phone/public call office",
        113 => "Facsimile/data (M.1081)",
        120 => "No remaining ACS sequential transmission",
        121 => "1 time remaining ACS sequential transmission",
        122 => "2 times remaining ACS sequential transmission",
        123 => "3 times remaining ACS sequential transmission",
        124 => "4 times remaining ACS sequential transmission",
        125 => "5 times remaining ACS sequential transmission",
        NO_INFO => "No information",
        _ => "Unknown",
    }
}

pub fn eos_name(v: u8) -> &'static str {
    match v {
        EOS_ACK_RQ => "Acknowledge RQ",
        EOS_ACK_BQ => "Acknowledge BQ",
        EOS_OTHER => "End of sequence",
        _ => "Unknown",
    }
}

fn sym_text(s: &[Option<u8>]) -> String {
    s.iter().map(|v| v.map_or("?".to_string(), |v| v.to_string())).collect::<Vec<_>>().join(" ")
}

impl DscCall {
    /// What kind of call this is, from its format, category, first
    /// telecommand and EOS.
    pub fn call_type(&self) -> &'static str {
        if self.format == FMT_DISTRESS {
            return "distressAlert";
        }
        if self.category == Some(CAT_DISTRESS) || self.distress.is_some() {
            match self.tc1 {
                Some(TC_DISTRESS_ACK) => {
                    let own = matches!((&self.distress, &self.from), (Some(Distress { id: Some(DistressId::Known(d)), .. }), Some(f)) if d == f);
                    return if own { "distressSelfCancel" } else { "distressAck" };
                }
                Some(TC_DISTRESS_RELAY) => {
                    return if self.eos == Some(EOS_ACK_BQ) { "distressRelayAck" } else { "distressRelay" };
                }
                _ => {}
            }
        }
        if self.tc1 == Some(118) {
            return "testCall";
        }
        match self.format {
            FMT_ALL_SHIPS => "allShipsCall",
            FMT_AREA => "areaCall",
            FMT_GROUP => "groupCall",
            FMT_INDIVIDUAL => "individualCall",
            FMT_AUTOMATIC => "automaticCall",
            _ => "call",
        }
    }

    fn label(&self) -> &'static str {
        match self.call_type() {
            "distressAlert" => "DISTRESS ALERT",
            "distressAck" => "DISTRESS ACK",
            "distressSelfCancel" => "DISTRESS SELF-CANCEL",
            "distressRelay" => "DISTRESS RELAY",
            "distressRelayAck" => "DISTRESS RELAY ACK",
            "testCall" => "TEST",
            "allShipsCall" => "ALL SHIPS",
            "areaCall" => "AREA",
            "groupCall" => "GROUP",
            "individualCall" => "INDIVIDUAL",
            "automaticCall" => "AUTOMATIC",
            _ => "CALL",
        }
    }

    pub fn summary(&self) -> String {
        let mut s = self.label().to_string();
        if let Some(c) = self.category
            && self.distress.is_none()
        {
            let _ = write!(s, " {}", category_name(c));
        }
        let _ = write!(s, " from {}", self.from.as_ref().map_or("?".into(), |m| m.digits.clone()));
        match &self.to {
            Some(Address::Station(m)) => {
                let _ = write!(s, " to {}", m.digits);
            }
            Some(Address::Area(a)) => {
                let _ = write!(s, " to area {}", a.text());
            }
            Some(Address::AllStations) => {}
            None if self.format == FMT_DISTRESS || self.format == FMT_ALL_SHIPS => {}
            None => s.push_str(" to ?"),
        }
        let mut parts: Vec<String> = Vec::new();
        if let Some(d) = &self.distress {
            if self.format != FMT_DISTRESS {
                parts.push(match &d.id {
                    Some(DistressId::Known(m)) => format!("ship in distress {}", m.digits),
                    Some(DistressId::Unknown) => "ship in distress unknown".into(),
                    None => "ship in distress ?".into(),
                });
            }
            parts.push(d.nature.map_or("nature ?".into(), |n| nature_name(n).to_string()));
            if let Some(p) = &d.position {
                parts.push(format!("position {}", p.text()));
            }
            if let Some(t) = &d.time {
                parts.push(format!("at {}", t.text()));
            }
            if let Some(c) = d.subsequent {
                parts.push(tc1_name(c).to_string());
            }
        } else {
            if let Some(t) = self.tc1 {
                parts.push(tc1_name(t).to_string());
            }
            if let Some(t) = self.tc2.filter(|t| *t != NO_INFO) {
                parts.push(tc2_name(t).to_string());
            }
            if let Some(f) = &self.rx_freq {
                parts.push(format!("RX {}", f.text()));
            }
            if let Some(f) = &self.tx_freq {
                parts.push(format!("TX {}", f.text()));
            }
            if let Some(p) = &self.position {
                parts.push(format!("position {}", p.text()));
            }
            if let Some(t) = &self.time {
                parts.push(format!("at {}", t.text()));
            }
            if let Some(n) = &self.number {
                parts.push(format!("number {n}"));
            }
        }
        if !parts.is_empty() {
            let _ = write!(s, ": {}", parts.join(", "));
        }
        if let Some(e) = self.eos.filter(|e| *e != EOS_OTHER) {
            let _ = write!(s, "; {}", eos_name(e));
        }
        s.push_str(if !self.complete {
            "; INCOMPLETE"
        } else if self.ecc_ok {
            "; ECC ok"
        } else {
            "; ECC FAILED"
        });
        if !self.bad_fields.is_empty() {
            let _ = write!(s, "; INVALID {}", self.bad_fields.join(", "));
        }
        s
    }

    pub fn fields(&self) -> Vec<(&'static str, String)> {
        let mut f: Vec<(&'static str, String)> = vec![
            ("callType", self.call_type().into()),
            ("format", self.format.to_string()),
            ("formatName", format_name(self.format).into()),
        ];
        if let Some(c) = self.category {
            f.push(("category", c.to_string()));
            f.push(("categoryName", category_name(c).into()));
        }
        match &self.to {
            Some(Address::AllStations) => f.push(("to", "all".into())),
            Some(Address::Station(m)) => mmsi_fields(&mut f, m, ["to", "toKind", "toMid", "toDigit10"]),
            Some(Address::Area(a)) => {
                f.push(("to", "area".into()));
                f.push(("area", a.text()));
            }
            None => {}
        }
        if let Some(m) = &self.from {
            mmsi_fields(&mut f, m, ["from", "fromKind", "fromMid", "fromDigit10"]);
        }
        if let Some(t) = self.tc1 {
            f.push(("tc1", t.to_string()));
            f.push(("tc1Name", tc1_name(t).into()));
        }
        if let Some(t) = self.tc2 {
            f.push(("tc2", t.to_string()));
            f.push(("tc2Name", tc2_name(t).into()));
        }
        let mut position = self.position.as_ref();
        let mut time = self.time.as_ref();
        if let Some(d) = &self.distress {
            match &d.id {
                Some(DistressId::Known(m)) => mmsi_fields(&mut f, m, ["distressId", "distressIdKind", "distressIdMid", "distressIdDigit10"]),
                Some(DistressId::Unknown) => f.push(("distressId", "unknown".into())),
                None => {}
            }
            if let Some(n) = d.nature {
                f.push(("nature", n.to_string()));
                f.push(("natureName", nature_name(n).into()));
            }
            position = d.position.as_ref();
            time = d.time.as_ref();
            if let Some(c) = d.subsequent {
                f.push(("subsequent", c.to_string()));
                f.push(("subsequentName", tc1_name(c).into()));
            }
        }
        if let Some(p) = position {
            f.push(("position", p.text()));
            if let Some((lat, lon)) = p.lat_lon() {
                f.push(("lat", format!("{lat:.4}")));
                f.push(("lon", format!("{lon:.4}")));
            }
        }
        if let Some(t) = time {
            f.push(("time", t.text()));
        }
        if let Some(x) = &self.rx_freq {
            f.push(("rxFreq", x.text()));
            if let Freq::Hz(hz) = x {
                f.push(("rxFreqHz", hz.to_string()));
            }
        }
        if let Some(x) = &self.tx_freq {
            f.push(("txFreq", x.text()));
            if let Freq::Hz(hz) = x {
                f.push(("txFreqHz", hz.to_string()));
            }
        }
        if let Some(n) = &self.number {
            f.push(("number", n.clone()));
        }
        if !self.message.is_empty() {
            f.push(("message", sym_text(&self.message)));
        }
        if let Some(e) = self.eos {
            f.push(("eos", e.to_string()));
            f.push(("eosName", eos_name(e).into()));
        }
        f.push(("ecc", if self.ecc_ok { "ok" } else { "failed" }.into()));
        if !self.bad_fields.is_empty() {
            f.push(("badFields", self.bad_fields.join(",")));
        }
        f.push(("complete", self.complete.to_string()));
        f.push(("rxCopies", self.rx_used.to_string()));
        f.push(("unreadable", self.unreadable.to_string()));
        f.push(("symbols", sym_text(&self.symbols)));
        f.push(("centerHz", format!("{:.1}", self.center_hz)));
        if let Some(s) = self.snr_db {
            f.push(("snrDb", format!("{s:.1}")));
        }
        if self.reverse {
            f.push(("reverse", "true".into()));
        }
        f
    }
}

fn mmsi_fields(f: &mut Vec<(&'static str, String)>, m: &Mmsi, keys: [&'static str; 4]) {
    f.push((keys[0], m.digits.clone()));
    f.push((keys[1], m.kind().name().into()));
    if let Some(mid) = m.mid() {
        f.push((keys[2], mid.to_string()));
    }
    if m.digit10 != 0 {
        f.push((keys[3], m.digit10.to_string()));
    }
}

impl DscEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            DscEvent::Call(c) => c.call_type(),
        }
    }

    pub fn summary(&self) -> String {
        match self {
            DscEvent::Call(c) => c.summary(),
        }
    }

    pub fn fields(&self) -> Vec<(&'static str, String)> {
        match self {
            DscEvent::Call(c) => c.fields(),
        }
    }
}

/// Where a call's content puts its EOS, as far as the characters so far
/// tell.
#[derive(Debug, PartialEq)]
enum Layout {
    Pending,
    Eos(usize),
    Bad,
}

/// Index of the first content character after the address, if any.
fn after_address(fmt: u8) -> usize {
    if fmt == FMT_ALL_SHIPS { 2 } else { 7 }
}

/// Distress calls carry fourteen characters of distress information after
/// the first telecommand instead of a second telecommand and messages.
fn distress_layout(cat: Option<u8>, tc1: Option<u8>) -> bool {
    matches!(tc1, Some(TC_DISTRESS_ACK | TC_DISTRESS_RELAY)) && cat.is_none_or(|c| c == CAT_DISTRESS)
}

/// Turns decided characters into a call. `eos` is the EOS's index, or
/// None for a call cut off.
fn parse(s: &[Option<u8>], eos: Option<usize>) -> DscCall {
    let mut call = parse_fields(s, eos);
    call.bad_fields = bad_fields(&call, s, eos);
    call
}

fn is_tc1(v: u8) -> bool {
    matches!(v, 100 | 101 | 103..=106 | 109 | 110 | 112 | 113 | 115 | 118 | 121 | NO_INFO)
}

fn is_tc2(v: u8) -> bool {
    matches!(v, 100..=113 | 120..=126)
}

/// The fields of a parsed call whose symbols are not allowed where they
/// stand (Table A1-3, note 1: unassigned symbols are rejected).
fn bad_fields(call: &DscCall, s: &[Option<u8>], eos: Option<usize>) -> Vec<&'static str> {
    let end = eos.unwrap_or(s.len());
    let field = |i: usize, n: usize| &s[i.min(end)..(i + n).min(end)];
    // A digit field is bad when a readable symbol in it is not two digits.
    let not_digits = |f: &[Option<u8>]| f.iter().any(|v| v.is_some_and(|v| v >= 100));
    let not = |v: Option<u8>, ok: fn(u8) -> bool| v.is_some_and(|v| !ok(v));
    let mut bad = Vec::new();
    let fmt = call.format;
    let mut i = 2;
    if fmt != FMT_DISTRESS && fmt != FMT_ALL_SHIPS {
        let a = field(2, 5);
        if not_digits(a) || (fmt == FMT_AREA && a.len() == 5 && a.iter().all(Option::is_some) && call.to.is_none()) {
            bad.push("to");
        }
        i = 7;
    }
    if fmt != FMT_DISTRESS {
        if not(call.category, |v| matches!(v, 100 | 106 | 108 | 110 | 112)) {
            bad.push("category");
        }
        i += 1;
    }
    if not_digits(field(i, 5)) {
        bad.push("from");
    }
    if fmt != FMT_DISTRESS && not(call.tc1, is_tc1) {
        bad.push("tc1");
    }
    if let Some(d) = &call.distress {
        let at = if fmt == FMT_DISTRESS { 7 } else { i + 6 };
        if fmt != FMT_DISTRESS {
            let id = field(at - 5, 5);
            if not_digits(id) && !id.iter().all(|v| *v == Some(NO_INFO)) {
                bad.push("distressId");
            }
        }
        if not(d.nature, |v| matches!(v, 100..=110 | 112)) {
            bad.push("nature");
        }
        if not_digits(field(at + 1, 5)) || matches!(d.position, Some(Position::Invalid(_))) {
            bad.push("position");
        }
        if not_digits(field(at + 6, 2)) || matches!(d.time, Some(Utc::Invalid(_))) {
            bad.push("time");
        }
        if not(d.subsequent, is_tc1) {
            bad.push("subsequent");
        }
    } else {
        if not(call.tc2, is_tc2) {
            bad.push("tc2");
        }
        let number = fmt == FMT_AUTOMATIC;
        if call.message.iter().any(|v| v.is_some_and(|v| v >= 100 && v != NO_INFO && !(number && (v == 105 || v == 106)))) {
            bad.push("message");
        }
        if matches!(call.position, Some(Position::Invalid(_))) {
            bad.push("position");
        }
        if matches!(call.time, Some(Utc::Invalid(_))) {
            bad.push("time");
        }
    }
    if eos.is_some() && call.eos.is_some_and(|e| !is_eos(e)) {
        bad.push("eos");
    }
    bad
}

fn parse_fields(s: &[Option<u8>], eos: Option<usize>) -> DscCall {
    let fmt = s[0].unwrap_or(0);
    let end = eos.unwrap_or(s.len());
    let get = |i: usize, n: usize| -> &[Option<u8>] { &s[i.min(end)..(i + n).min(end)] };
    let one = |i: usize| -> Option<u8> { if i < end { s[i] } else { None } };
    let mut call = DscCall {
        format: fmt,
        category: None,
        to: None,
        from: None,
        tc1: None,
        tc2: None,
        distress: None,
        rx_freq: None,
        tx_freq: None,
        position: None,
        time: None,
        number: None,
        message: Vec::new(),
        eos: eos.and_then(|e| s[e]),
        ecc_ok: false,
        bad_fields: Vec::new(),
        complete: eos.is_some(),
        rx_used: 0,
        unreadable: 0,
        symbols: s.to_vec(),
        center_hz: 0.0,
        snr_db: None,
        reverse: false,
    };
    let distress_tail = |i: usize| Distress {
        id: None,
        nature: one(i),
        position: Position::from_symbols(get(i + 1, 5)).filter(|_| get(i + 1, 5).len() == 5),
        time: Utc::from_symbols(get(i + 6, 2)),
        subsequent: one(i + 8),
    };
    if fmt == FMT_DISTRESS {
        call.to = Some(Address::AllStations);
        call.from = Mmsi::from_symbols(get(2, 5));
        let mut d = distress_tail(7);
        d.id = call.from.clone().map(DistressId::Known);
        call.distress = Some(d);
        return call;
    }
    let mut i = 2;
    if fmt == FMT_ALL_SHIPS {
        call.to = Some(Address::AllStations);
    } else {
        let a = get(2, 5);
        call.to = if fmt == FMT_AREA { Area::from_symbols(a).map(Address::Area) } else { Mmsi::from_symbols(a).map(Address::Station) };
        i = 7;
    }
    call.category = one(i);
    call.from = Mmsi::from_symbols(get(i + 1, 5));
    call.tc1 = one(i + 6);
    i += 7;
    if distress_layout(call.category, call.tc1) {
        let id = get(i, 5);
        let mut d = distress_tail(i + 5);
        d.id = if id.len() == 5 && id.iter().all(|v| *v == Some(NO_INFO)) {
            Some(DistressId::Unknown)
        } else {
            Mmsi::from_symbols(id).map(DistressId::Known)
        };
        call.distress = Some(d);
        return call;
    }
    call.tc2 = one(i);
    i += 1;
    let m = get(i, 32);
    call.message = m.to_vec();
    if call.tc1 == Some(TC_POSITION) {
        // A position request is six 126s; the reply is the position and a
        // 126, then the time (Table A1-7).
        if m.len() >= 5 && m[..5].iter().any(|v| *v != Some(NO_INFO)) {
            call.position = Position::from_symbols(&m[..5]);
            call.time = m.get(6..8).and_then(Utc::from_symbols);
        }
        return call;
    }
    let mut j = 0;
    if m.first() == Some(&Some(POSITION_MARK)) {
        // A call carrying the ship's position in place of a frequency.
        if m.len() >= 6 {
            call.position = Position::from_symbols(&m[1..6]);
        }
        j = 6;
    } else {
        let mut first_len = 3;
        for k in 0..2 {
            let Some((f, n)) = freq_element(&m[j.min(m.len())..], first_len) else { break };
            if k == 0 {
                call.rx_freq = f;
                first_len = n.max(3);
            } else {
                call.tx_freq = f;
            }
            j += n;
        }
    }
    if fmt == FMT_AUTOMATIC && j < m.len() {
        let rest = &m[j..];
        if let Some(lead @ (105 | 106)) = rest[0]
            && let Some(d) = digits(&rest[1..])
        {
            // 105 says the number has an odd count of digits, padded with
            // a leading zero to fill the first symbol.
            let mut n: String = d.iter().map(|x| char::from(b'0' + x)).collect();
            if lead == 105 && !n.is_empty() {
                n.remove(0);
            }
            call.number = Some(n);
        }
    }
    call
}

/// The layout so far of a call's decided characters.
fn layout(s: &[Option<u8>], eos_at: impl Fn(usize) -> Option<bool>) -> Layout {
    if s.len() < 2 {
        return Layout::Pending;
    }
    let Some(fmt) = s[0].filter(|v| known_format(*v)) else { return Layout::Bad };
    if fmt == FMT_DISTRESS {
        return Layout::Eos(16);
    }
    let p = after_address(fmt);
    if s.len() <= p + 6 {
        return Layout::Pending;
    }
    if distress_layout(s[p], s[p + 6]) {
        return Layout::Eos(p + 7 + 14);
    }
    // After the second telecommand, the messages hold digits, 126, 55
    // and 105/106, never an EOS: the first EOS is the end.
    for (i, v) in s.iter().enumerate().skip(p + 8) {
        match *v {
            Some(v) if is_eos(v) => return Layout::Eos(i),
            Some(_) => {}
            None => match eos_at(i) {
                Some(true) => return Layout::Eos(i),
                Some(false) => {}
                None => return Layout::Pending,
            },
        }
        if i > p + 8 + 30 {
            return Layout::Bad;
        }
    }
    Layout::Pending
}

// ---------------------------------------------------------------------------
// The decoder.

/// A call being received.
struct CallRx {
    inverted: bool,
    /// Bit at which phasing slot 0 ended.
    origin: u64,
    /// The last slot taken.
    slot: u64,
    dx: Vec<Soft>,
    rx: Vec<Soft>,
    /// Whether each slot from 12 on looked like signal.
    good: Vec<bool>,
    /// Slots from this one on are not used: the signal had gone.
    cut: usize,
    /// Each character's decision from its DX and RX copies.
    dec: Vec<Option<u8>>,
}

impl CallRx {
    /// A character from those of its copies (DX indexes in `dx`, RX in
    /// `rx`) heard before any cut; unreadable when there are none.
    fn decide(&self, dx: &[usize], rx: &[usize]) -> Option<u8> {
        let mut copies: Vec<&Soft> = Vec::with_capacity(4);
        copies.extend(dx.iter().filter(|&&d| 12 + 2 * d < self.cut).map(|&d| &self.dx[d]));
        copies.extend(rx.iter().filter(|&&d| 17 + 2 * d < self.cut).map(|&d| &self.rx[d]));
        if copies.is_empty() { None } else { combine(&copies) }
    }

    /// Whether the last `n` slots say the signal has gone.
    fn gone(&self, n: usize) -> bool {
        n > 0 && self.good.len() >= n && self.good.iter().rev().take(n).filter(|g| **g).count() as f32 <= GONE_SHARE * n as f32
    }

    /// The earliest slot (from 12) of the longest stretch up to the end
    /// that says the signal has gone, from its first slot without signal.
    fn gone_since(&self) -> Option<usize> {
        let n = self.good.len();
        let start = (CUT_SLOTS..=n).filter(|&len| self.gone(len)).max().map(|len| n - len)?;
        let first_bad = (start..n).find(|&k| !self.good[k])?;
        Some(12 + first_bad)
    }

    /// The call did not close: drop the copies heard after the signal went,
    /// which the fading correction reads as confidently as signal, and
    /// decide again without them.
    fn cut_off(&mut self) {
        let Some(cut) = self.gone_since() else { return };
        self.cut = cut;
        for d in 0..self.dec.len() {
            self.dec[d] = self.decide(&[d], &[d]);
        }
        if self.dec.len() >= 2 {
            let f = self.decide(&[0, 1], &[0, 1]);
            self.dec[0] = f;
            self.dec[1] = f;
        }
    }
}

pub struct DscDecoder {
    front: FrontEnd,
    spec: Spectrum,
    demod: Demod,
    hist: Vec<Complex32>,
    /// Band samples taken in so far; also the index of the next one.
    n2: u64,
    /// Center of the pair as an offset from 1700 Hz, while locked.
    center: Option<f32>,
    present: bool,
    absent: u32,
    hold_frames: u32,
    cand: Option<f32>,
    cand_n: u32,
    snr_db: Option<f32>,
    /// Band sample after the end of the last call: a replay starts no
    /// earlier, so a finished call is not heard twice.
    replay_floor: u64,
    // Bit clock.
    atc: Atc,
    spb: f32,
    half_len: f32,
    ph: f32,
    prev: (f32, f32, f32),
    have_prev: bool,
    last_v: f32,
    // Characters.
    soft: [f32; RING],
    /// Each bit's stronger and weaker tone power.
    tone_pow: [(f32, f32); RING],
    /// The symbol the ten bits ending at each bit are, in each polarity.
    syms: [[Option<u8>; 2]; RING],
    nbits: u64,
    call: Option<CallRx>,
    symbols: u32,
    reverse: bool,
    calls: u32,
    events: Vec<DscEvent>,
}

/// The phasing character expected in each of the sixteen slots from the
/// first DX phasing character; the format specifier fills slots 12 and 14.
fn phasing(slot: usize) -> Option<u8> {
    match slot {
        0..=10 if slot.is_multiple_of(2) => Some(DX_PHASING),
        1..=15 if slot % 2 == 1 => Some(111 - (slot as u8 - 1) / 2),
        _ => None,
    }
}

impl DscDecoder {
    pub fn new(fs: f32) -> Self {
        let decim = ((fs / BAND_RATE).round() as usize).max(1);
        let fs2 = fs / decim as f32;
        let spec = Spectrum::new(fs2);
        let demod = Demod::new(fs2);
        let spb = demod.fs3 / BAUD;
        let half_len = 0.5 * demod.ring_hi.len() as f32;
        let hold_frames = (HOLD_S * fs2 / spec.hop as f32) as u32;
        Self {
            front: FrontEnd::new(fs, decim),
            hist: vec![Complex32::new(0.0, 0.0); (HISTORY_S * fs2) as usize],
            spec,
            demod,
            n2: 0,
            center: None,
            present: false,
            absent: 0,
            hold_frames,
            cand: None,
            cand_n: 0,
            snr_db: None,
            replay_floor: 0,
            atc: Atc::new(spb),
            spb,
            half_len,
            ph: spb,
            prev: (0.0, 0.0, 0.0),
            have_prev: false,
            last_v: 0.0,
            soft: [0.0; RING],
            tone_pow: [(0.0, 0.0); RING],
            syms: [[None; 2]; RING],
            nbits: 0,
            call: None,
            symbols: 0,
            reverse: false,
            calls: 0,
            events: Vec::new(),
        }
    }

    pub fn push(&mut self, audio: &[f32]) {
        let cap = self.hist.len() as u64;
        for &x in audio {
            let Some(z) = self.front.push(x) else { continue };
            let i = self.n2;
            self.n2 += 1;
            self.hist[(i % cap) as usize] = z;
            if self.spec.push(z) && self.evaluate(i) {
                continue;
            }
            if self.center.is_some() {
                self.demod_sample(z, i);
            }
        }
    }

    pub fn take(&mut self) -> Vec<DscEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn status(&self) -> DscStatus {
        DscStatus {
            locked: self.center.is_some() && self.present,
            center_hz: self.center.map(|c| AUDIO_CENTER_HZ + c),
            snr_db: self.center.and(self.snr_db),
            receiving: self.call.is_some(),
            symbols: self.call.as_ref().map_or(self.symbols, |c| c.dx.len() as u32),
            reverse: self.reverse,
            calls: self.calls,
        }
    }

    /// A new spectrum: lock, follow or let go of the pair. True when the
    /// history up to and including band sample `i` was demodulated anew.
    fn evaluate(&mut self, i: u64) -> bool {
        if self.spec.frames < MIN_FRAMES {
            return false;
        }
        let best = self.spec.best().filter(|(_, s)| *s > ACQUIRE_RHO);
        match best {
            Some((c, _)) => {
                self.cand_n = match self.cand {
                    Some(p) if (p - c).abs() <= RECENTER_HZ => self.cand_n + 1,
                    _ => 1,
                };
                self.cand = Some(c);
            }
            None => {
                self.cand = None;
                self.cand_n = 0;
            }
        }
        let confirmed = best.filter(|_| self.cand_n >= CONFIRM).map(|(c, _)| c);
        let in_call = self.call.is_some();
        let Some(cur) = self.center else {
            if let Some(c) = confirmed {
                self.lock(c, i);
                return true;
            }
            return false;
        };
        let p = self.spec.pair_at(cur);
        self.present = p.score() > HOLD_RHO;
        if self.present {
            self.absent = 0;
            self.snr_db = Some(self.spec.snr_db(&p));
        } else {
            self.absent += 1;
        }
        if in_call {
            // The center holds still through a call.
            return false;
        }
        if let Some(c) = confirmed {
            if (c - cur).abs() > RECENTER_HZ {
                self.lock(c, i);
                return true;
            }
            let c = cur + 0.3 * (c - cur);
            self.center = Some(c);
            self.demod.set_center(c);
        } else if self.absent > self.hold_frames {
            self.center = None;
            self.snr_db = None;
        }
        false
    }

    /// Lock at `c` and demodulate the history up to band sample `i`.
    fn lock(&mut self, c: f32, i: u64) {
        self.center = Some(c);
        self.present = true;
        self.absent = 0;
        self.snr_db = Some(self.spec.snr_db(&self.spec.pair_at(c)));
        self.demod.reset(c);
        self.restart_bits();
        let cap = self.hist.len() as u64;
        let start = (i + 1).saturating_sub(cap).max(self.replay_floor);
        for j in start..=i {
            let z = self.hist[(j % cap) as usize];
            self.demod_sample(z, j);
        }
    }

    fn restart_bits(&mut self) {
        self.atc.reset();
        self.ph = self.spb;
        self.have_prev = false;
        self.last_v = 0.0;
        self.syms = [[None; 2]; RING];
        self.nbits = 0;
        self.call = None;
    }

    fn demod_sample(&mut self, z: Complex32, i: u64) {
        let Some((hi, lo)) = self.demod.push(z) else { return };
        let cur = (self.atc.step(hi, lo), hi, lo);
        if !self.have_prev {
            self.prev = cur;
            self.have_prev = true;
            return;
        }
        let prev = std::mem::replace(&mut self.prev, cur);
        self.ph -= 1.0;
        if (prev.0 > 0.0) != (cur.0 > 0.0) {
            // The matched filter crosses zero half a filter after an edge
            // and holds a whole bit half a filter after that.
            let back = 1.0 - prev.0 / (prev.0 - cur.0);
            let mut e = self.ph + back - self.half_len;
            e -= self.spb * (e / self.spb).round();
            // A crossing after a doubtful bit is as likely noise.
            let w = self.last_v.abs().min(1.0);
            let g = if self.call.is_some() { CLOCK_GAIN_SYNC } else { CLOCK_GAIN_HUNT };
            self.ph -= g * w * e;
        }
        while self.ph <= 0.0 {
            let a = 1.0 + self.ph;
            let at = |p: f32, c: f32| p + (c - p) * a;
            let (v, h, l) = (at(prev.0, cur.0), at(prev.1, cur.1), at(prev.2, cur.2));
            self.ph += self.spb;
            self.bit(v, h, l, i);
        }
    }

    /// One bit; `i` is the band sample it came out of.
    fn bit(&mut self, v: f32, hi: f32, lo: f32, i: u64) {
        self.last_v = v;
        let t = self.nbits;
        let (a, b) = (hi * hi, lo * lo);
        self.tone_pow[(t % RING as u64) as usize] = (a.max(b), a.min(b));
        // The upper tone is B, binary 0: the ratio is positive for Y.
        self.soft[(t % RING as u64) as usize] = (-v * self.atc.llr_scale()).clamp(-LLR_MAX, LLR_MAX);
        self.nbits += 1;
        if t < 9 {
            return;
        }
        let w = hard(&self.word(t, false));
        let s = [decode_hard(w), decode_hard(!w & 0x3ff)];
        self.syms[(t % RING as u64) as usize] = s;
        for (k, inverted) in [(0, false), (1, true)] {
            if let Some(origin) = self.phasing_at(t, s[k], k) {
                let same = self.call.as_ref().is_some_and(|c| c.inverted == inverted && c.origin == origin);
                if !same {
                    self.start_call(origin, inverted, t, i);
                }
            }
        }
        let due = self.call.as_ref().and_then(|c| (t == c.origin + 10 * (c.slot + 1)).then_some(c.slot + 1));
        if let Some(k) = due {
            self.slot(k, t, i);
        }
    }

    /// The ten soft bits ending at bit `t`, in the order sent.
    fn word(&self, t: u64, inverted: bool) -> Soft {
        let sign = if inverted { -1.0 } else { 1.0 };
        let mut w = [0.0; 10];
        for (j, x) in w.iter_mut().enumerate() {
            *x = sign * self.soft[((t - 9 + j as u64) % RING as u64) as usize];
        }
        w
    }

    /// Whether the slot ending at bit `t` held signal: the Atc reads noise
    /// as confidently as signal once the signal has gone, so a character
    /// copy from a slot without signal is not used at all.
    fn slot_good(&self, t: u64) -> bool {
        let (mut strong, mut weak) = (0.0f32, 0.0f32);
        for j in 0..10 {
            let (s, w) = self.tone_pow[((t - j) % RING as u64) as usize];
            strong += s;
            weak += w;
        }
        strong - weak > GOOD_Q * weak.max(TINY)
    }

    /// Phasing (M.493 § 3.3) completed by the character ending at bit `t`:
    /// the bit at which phasing slot 0 ended.
    fn phasing_at(&self, t: u64, sym: Option<u8>, pol: usize) -> Option<u64> {
        let sym = sym?;
        let slots: &[usize] = match sym {
            DX_PHASING => &[0, 2, 4, 6, 8, 10],
            104..=111 => &[1, 3, 5, 7, 9, 11, 13, 15][(111 - sym) as usize..][..1],
            _ => return None,
        };
        for &s in slots {
            let back = 10 * s as u64;
            if t < back + 9 {
                continue;
            }
            let (mut dx, mut rx) = (0, 0);
            for s2 in 0..=s {
                let Some(want) = phasing(s2) else { continue };
                let at = t - 10 * (s - s2) as u64;
                if self.syms[(at % RING as u64) as usize][pol] == Some(want) {
                    if s2.is_multiple_of(2) {
                        dx += 1;
                    } else {
                        rx += 1;
                    }
                }
            }
            if (dx >= 2 && rx >= 1) || (rx >= 2 && dx >= 1) || rx >= 3 {
                return Some(t - back);
            }
        }
        None
    }

    fn start_call(&mut self, origin: u64, inverted: bool, t: u64, i: u64) {
        if let Some(c) = self.call.take() {
            self.finish(c, None, i);
        }
        self.reverse = inverted;
        let mut c = CallRx {
            inverted,
            origin,
            slot: 11,
            dx: Vec::new(),
            rx: Vec::new(),
            good: Vec::new(),
            cut: usize::MAX,
            dec: Vec::new(),
        };
        // The format specifier slots that went by before phasing was
        // complete (phasing can end as late as slot 15).
        let last = (t - origin) / 10;
        for k in 12..=last {
            let w = self.word(origin + 10 * k, inverted);
            c.good.push(self.slot_good(origin + 10 * k));
            if k.is_multiple_of(2) {
                c.dx.push(w);
            }
            c.slot = k;
        }
        self.call = Some(c);
    }

    /// Slot `k` of the call ended at bit `t`.
    fn slot(&mut self, k: u64, t: u64, i: u64) {
        let Some(mut c) = self.call.take() else { return };
        c.slot = k;
        let w = self.word(t, c.inverted);
        c.good.push(self.slot_good(t));
        if k.is_multiple_of(2) {
            c.dx.push(w);
        } else if k >= 17 {
            let d = c.rx.len();
            c.rx.push(w);
            let v = c.decide(&[d], &[d]);
            c.dec.push(v);
            if d == 1 {
                // The format specifier: four copies.
                let f = c.decide(&[0, 1], &[0, 1]);
                c.dec[0] = f;
                c.dec[1] = f;
            }
        }
        if c.gone(LOST_SLOTS) {
            self.finish(c, None, i);
            return;
        }
        let lay = layout(&c.dec, |e| {
            // An unreadable EOS candidate: its four copies decide, once the
            // last two have come.
            (c.dx.len() > e + 3).then(|| c.decide(&[e, e + 2, e + 3], &[e]).is_some_and(is_eos))
        });
        match lay {
            Layout::Eos(e) if c.rx.len() >= e + 2 && c.dx.len() >= e + 4 => {
                c.dec[e] = c.decide(&[e, e + 2, e + 3], &[e]);
                // Where the layout puts the EOS, an EOS must be: otherwise
                // the closing sequence was lost and the call is cut off.
                let closed = c.dec[e].is_some_and(is_eos);
                self.finish(c, closed.then_some(e), i);
            }
            Layout::Bad => self.finish(c, None, i),
            _ if c.dx.len() > MAX_DX => self.finish(c, None, i),
            _ => self.call = Some(c),
        }
    }

    /// Report a call, complete through its ECC (`eos` given) or cut off.
    fn finish(&mut self, mut c: CallRx, eos: Option<usize>, i: u64) {
        if eos.is_none() {
            c.cut_off();
        }
        self.symbols = c.dx.len() as u32;
        self.replay_floor = i + 1;
        // A call is only worth reporting once its format specifier is
        // known, and a distress or all-ships format must have been seen
        // twice (§ 4.2).
        let Some(fmt) = c.dec.first().copied().flatten().filter(|f| known_format(*f)) else { return };
        if matches!(fmt, FMT_DISTRESS | FMT_ALL_SHIPS) {
            let seen = [c.dx.first(), c.dx.get(1), c.rx.first(), c.rx.get(1)]
                .iter()
                .filter(|w| w.is_some_and(|w| decode_hard(hard(w)) == Some(fmt)))
                .count();
            if seen < 2 {
                return;
            }
        }
        let n = eos.map_or(c.dec.len(), |e| e + 2);
        let s = &c.dec[..n];
        let mut call = parse(s, eos);
        if let Some(e) = eos {
            let ecc = s[0].and_then(|f| s[2..=e].iter().try_fold(f, |a, v| v.map(|v| a ^ v)));
            call.ecc_ok = ecc.is_some() && ecc == s[e + 1];
        }
        for (d, v) in s.iter().enumerate() {
            match v {
                None => call.unreadable += 1,
                Some(v) if decode_hard(hard(&c.dx[d])) != Some(*v) => call.rx_used += 1,
                _ => {}
            }
        }
        call.center_hz = AUDIO_CENTER_HZ + self.center.unwrap_or(0.0);
        call.snr_db = self.snr_db;
        call.reverse = c.inverted;
        self.calls += 1;
        self.events.push(DscEvent::Call(call));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtty::testsig::*;

    /// What the live server delivers.
    const FS: f32 = 47_904.0;

    #[test]
    fn dsc_code_table_matches_m493() {
        // Table A1-1, bit 1 first, B = 0 and Y = 1.
        let table = [
            (0u8, "BBBBBBBYYY"),
            (1, "YBBBBBBYYB"),
            (42, "BYBYBYBYBB"),
            (85, "YBYBYBYBYY"),
            (100, "BBYBBYYYBB"),
            (104, "BBBYBYYYBB"),
            (111, "YYYYBYYBBY"),
            (117, "YBYBYYYBYB"),
            (125, "YBYYYYYBBY"),
            (126, "BYYYYYYBBY"),
            (127, "YYYYYYYBBB"),
        ];
        for (v, s) in table {
            let w: u16 = s.bytes().enumerate().fold(0, |w, (j, c)| w | (u16::from(c == b'Y') << j));
            assert_eq!(CODEWORDS[v as usize], w, "symbol {v}");
            assert_eq!(decode_hard(w), Some(v));
        }
        // The dot pattern never frames as a character.
        assert_eq!(decode_hard(0b0101010101), None);
        assert_eq!(decode_hard(0b1010101010), None);
        assert_eq!((0..1024u16).filter(|w| decode_hard(*w).is_some()).count(), 128);
    }

    /// A call's DX characters, from its content (format specifier once,
    /// through the EOS): the format specifier twice, the ECC, and the EOS
    /// twice more.
    fn dx_sequence(content: &[u8]) -> Vec<u8> {
        let (fmt, eos) = (content[0], *content.last().unwrap());
        let ecc = content.iter().fold(0, |a, v| a ^ v);
        let mut dx = vec![fmt];
        dx.extend_from_slice(content);
        dx.extend([ecc, eos, eos]);
        dx
    }

    /// Every slot of the emission after the dot pattern: phasing, then each
    /// DX character in slot 12 + 2d and again in slot 17 + 2d.
    fn slots(dx: &[u8]) -> Vec<u8> {
        let n = 12 + 2 * dx.len();
        let mut out = vec![0u8; n];
        for (s, o) in out.iter_mut().enumerate().take(16) {
            if let Some(p) = phasing(s) {
                *o = p;
            }
        }
        for (d, &v) in dx.iter().enumerate() {
            out[12 + 2 * d] = v;
            if 17 + 2 * d < n {
                out[17 + 2 * d] = v;
            }
        }
        out
    }

    /// Tone levels (true for the upper tone, B, binary 0) for a dot
    /// pattern and the slots.
    fn levels(slots: &[u8], dots: usize) -> Vec<bool> {
        let mut lv: Vec<bool> = (0..dots).map(|i| i % 2 == 0).collect();
        for &s in slots {
            let w = CODEWORDS[s as usize];
            lv.extend((0..10).map(|j| w >> j & 1 == 0));
        }
        lv
    }

    struct Tx {
        offset: f32,
        ppm: f32,
        snr: f32,
        ch: Channel,
        invert: bool,
        dots: usize,
    }

    impl Default for Tx {
        fn default() -> Self {
            Tx { offset: 0.0, ppm: 0.0, snr: 10.0, ch: Channel::Awgn, invert: false, dots: 200 }
        }
    }

    /// Audio for calls back to back, each through `tx`, with a second of
    /// noise before and after.
    fn audio(level_runs: &[Vec<bool>], tx: &Tx, seed: u64) -> Vec<f32> {
        let amp = 0.1;
        let mut rng = Rng::new(seed);
        // A sample clock off by `ppm` scales every rate and frequency.
        let k = 1.0 + tx.ppm * 1e-6;
        let mut x = vec![0.0; FS as usize];
        for lv in level_runs {
            let lv: Vec<bool> = lv.iter().map(|b| *b != tx.invert).collect();
            x.extend(fsk_through(&lv, BAUD * k, FS, (AUDIO_CENTER_HZ + tx.offset) * k, SHIFT_HZ * k, amp, tx.ch, &mut rng));
        }
        x.extend(vec![0.0; (1.5 * FS) as usize]);
        add_noise(&mut x, amp, tx.snr, FS, &mut rng);
        x
    }

    fn run(x: &[f32]) -> (Vec<DscCall>, DscStatus) {
        let mut d = DscDecoder::new(FS);
        let mut out = Vec::new();
        let mut st = None;
        chunks(x, |c| {
            d.push(c);
            let ev = d.take();
            if st.is_none() && !ev.is_empty() {
                st = Some(d.status());
            }
            out.extend(ev.into_iter().map(|DscEvent::Call(c)| c));
        });
        (out, st.unwrap_or_else(|| d.status()))
    }

    fn mmsi(d: &str) -> [u8; 5] {
        let d = format!("{d}0");
        let b = d.as_bytes();
        std::array::from_fn(|i| 10 * (b[2 * i] - b'0') + (b[2 * i + 1] - b'0'))
    }

    fn cat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    const SHIP: &str = "366123450";
    const SHIP2: &str = "211234560";
    const COAST: &str = "002320001";

    /// Distress alert: fire, 40-30N 073-15W at 14:32, J3E telephony.
    fn distress_alert() -> Vec<u8> {
        cat(&[&[FMT_DISTRESS], &mmsi(SHIP), &[100, 14, 3, 0, 73, 15, 14, 32, 109, EOS_OTHER]])
    }

    /// Individual routine call, J3E telephony on 8291.0 kHz, ack requested.
    fn individual_call() -> Vec<u8> {
        cat(&[&[FMT_INDIVIDUAL], &mmsi(SHIP2), &[100], &mmsi(SHIP), &[109, 126, 8, 29, 10, 8, 29, 10, EOS_ACK_RQ]])
    }

    /// A coast station's all-ships safety call: J3E on 4125.0 kHz, receive
    /// frequency only.
    fn all_ships_safety() -> Vec<u8> {
        cat(&[&[FMT_ALL_SHIPS, 108], &mmsi(COAST), &[109, 126, 4, 12, 50, 126, 126, 126, EOS_OTHER]])
    }

    fn check_alert(c: &DscCall) {
        assert_eq!(c.call_type(), "distressAlert");
        assert!(c.complete && c.ecc_ok, "{}", c.summary());
        assert_eq!(c.from.as_ref().unwrap().digits, SHIP);
        assert_eq!(c.from.as_ref().unwrap().mid(), Some(366));
        let d = c.distress.as_ref().unwrap();
        assert_eq!(d.nature, Some(100));
        assert_eq!(d.position, Some(Position::Known { north: true, east: false, lat_deg: 40, lat_min: 30, lon_deg: 73, lon_min: 15 }));
        assert_eq!(d.time, Some(Utc::Known(14, 32)));
        assert_eq!(d.subsequent, Some(109));
        assert_eq!(c.eos, Some(EOS_OTHER));
    }

    fn check_individual(c: &DscCall) {
        assert_eq!(c.call_type(), "individualCall");
        assert!(c.complete && c.ecc_ok, "{}", c.summary());
        assert_eq!(c.to, Some(Address::Station(Mmsi { digits: SHIP2.into(), digit10: 0 })));
        assert_eq!(c.from.as_ref().unwrap().digits, SHIP);
        assert_eq!((c.category, c.tc1, c.tc2), (Some(100), Some(109), Some(126)));
        assert_eq!(c.rx_freq, Some(Freq::Hz(8_291_000)));
        assert_eq!(c.tx_freq, Some(Freq::Hz(8_291_000)));
        assert_eq!(c.eos, Some(EOS_ACK_RQ));
    }

    fn check_all_ships(c: &DscCall) {
        assert_eq!(c.call_type(), "allShipsCall");
        assert!(c.complete && c.ecc_ok, "{}", c.summary());
        assert_eq!(c.to, Some(Address::AllStations));
        let from = c.from.as_ref().unwrap();
        assert_eq!((from.digits.as_str(), from.kind(), from.mid()), (COAST, MmsiKind::Coast, Some(232)));
        assert_eq!((c.category, c.tc1, c.tc2), (Some(108), Some(109), Some(126)));
        assert_eq!(c.rx_freq, Some(Freq::Hz(4_125_000)));
        assert_eq!(c.tx_freq, None);
        assert_eq!(c.eos, Some(EOS_OTHER));
    }

    fn one_call(content: &[u8], tx: &Tx, seed: u64) -> (Vec<DscCall>, DscStatus) {
        let lv = levels(&slots(&dx_sequence(content)), tx.dots);
        run(&audio(&[lv], tx, seed))
    }

    #[test]
    fn dsc_clean_calls_decode() {
        let tx = Tx { snr: 20.0, ..Tx::default() };
        let (c, st) = one_call(&distress_alert(), &tx, 1);
        assert_eq!(c.len(), 1, "{c:#?}");
        check_alert(&c[0]);
        assert_eq!((c[0].rx_used, c[0].unreadable), (0, 0));
        assert!((st.center_hz.unwrap() - 1700.0).abs() < 3.0, "{st:?}");
        assert!(st.locked && !st.reverse);
        println!("{}", c[0].summary());
        println!("{:?}", c[0].fields());
        let (c, _) = one_call(&individual_call(), &tx, 2);
        assert_eq!(c.len(), 1, "{c:#?}");
        check_individual(&c[0]);
        println!("{}", c[0].summary());
        let (c, _) = one_call(&all_ships_safety(), &Tx { dots: 20, ..tx }, 3);
        assert_eq!(c.len(), 1, "{c:#?}");
        check_all_ships(&c[0]);
        println!("{}", c[0].summary());
    }

    #[test]
    fn dsc_every_layout_parses() {
        let tx = Tx { snr: 15.0, ..Tx::default() };
        let pos = [14, 3, 0, 73, 15];
        // Distress acknowledgment by a coast station.
        let ack = cat(&[&[FMT_ALL_SHIPS, 112], &mmsi(COAST), &[110], &mmsi(SHIP), &[100], &pos, &[14, 32, 109, EOS_OTHER]]);
        // Self-cancel: the ship acknowledges its own alert.
        let cancel = cat(&[&[FMT_ALL_SHIPS, 112], &mmsi(SHIP), &[110], &mmsi(SHIP), &[100], &pos, &[14, 32, 109, EOS_OTHER]]);
        // Relay to an area, NW corner 40N 075W, 10 by 20 degrees, for an
        // unknown station, position unavailable.
        let relay = cat(&[
            &[FMT_AREA, 14, 0, 75, 10, 20, 112],
            &mmsi(COAST),
            &[112, 126, 126, 126, 126, 126, 105, 99, 99, 99, 99, 99, 88, 88, 109, EOS_OTHER],
        ]);
        // Relay acknowledgment, individual.
        let relay_ack = cat(&[&[FMT_INDIVIDUAL], &mmsi(COAST), &[112], &mmsi(SHIP2), &[112], &mmsi(SHIP), &[101], &pos, &[14, 32, 109, EOS_ACK_BQ]]);
        // Group call, J3E on 6516.5 kHz in 10 Hz steps.
        let group = cat(&[&[FMT_GROUP], &mmsi("036699999"), &[100], &mmsi(SHIP), &[109, 126, 40, 65, 16, 50, 126, 126, 126, 126, EOS_OTHER]]);
        // Automatic call to a coast station with a number.
        let auto = cat(&[&[FMT_AUTOMATIC], &mmsi(COAST), &[100], &mmsi(SHIP), &[109, 126, 30, 4, 1, 126, 126, 126, 105, 0, 44, 20, 79, EOS_ACK_RQ]]);
        // Position reply.
        let posack = cat(&[&[FMT_INDIVIDUAL], &mmsi(COAST), &[108], &mmsi(SHIP), &[121, 126], &pos, &[126, 9, 5, EOS_ACK_BQ]]);
        // Test call.
        let test = cat(&[&[FMT_INDIVIDUAL], &mmsi(COAST), &[108], &mmsi(SHIP), &[118, 126, 126, 126, 126, 126, 126, 126, EOS_ACK_RQ]]);
        let calls = [&ack, &cancel, &relay, &relay_ack, &group, &auto, &posack, &test];
        let runs: Vec<Vec<bool>> = calls.iter().map(|c| levels(&slots(&dx_sequence(c)), 200)).collect();
        let (c, _) = run(&audio(&runs, &tx, 7));
        for x in &c {
            println!("{}", x.summary());
        }
        assert_eq!(c.len(), calls.len(), "{c:#?}");
        assert!(c.iter().all(|c| c.complete && c.ecc_ok));
        let kinds: Vec<&str> = c.iter().map(DscCall::call_type).collect();
        assert_eq!(
            kinds,
            ["distressAck", "distressSelfCancel", "distressRelay", "distressRelayAck", "groupCall", "automaticCall", "individualCall", "testCall"]
        );
        let d = c[0].distress.as_ref().unwrap();
        assert_eq!(d.id, Some(DistressId::Known(Mmsi { digits: SHIP.into(), digit10: 0 })));
        assert_eq!((d.nature, d.time.clone()), (Some(100), Some(Utc::Known(14, 32))));
        assert_eq!(c[0].from.as_ref().unwrap().kind(), MmsiKind::Coast);
        assert_eq!(c[2].to, Some(Address::Area(Area { north: true, east: false, lat_deg: 40, lon_deg: 75, dlat: 10, dlon: 20 })));
        let d = c[2].distress.as_ref().unwrap();
        assert_eq!((d.id.clone(), d.nature, d.position.clone(), d.time.clone()), (Some(DistressId::Unknown), Some(105), Some(Position::Unavailable), Some(Utc::Unavailable)));
        assert_eq!(c[3].distress.as_ref().unwrap().nature, Some(101));
        assert_eq!(c[3].eos, Some(EOS_ACK_BQ));
        let g = &c[4];
        assert_eq!(g.to.as_ref().map(|a| matches!(a, Address::Station(m) if m.kind() == MmsiKind::Group && m.mid() == Some(366))), Some(true));
        assert_eq!((g.rx_freq.clone(), g.tx_freq.clone()), (Some(Freq::Hz(6_516_500)), None));
        assert_eq!(c[5].rx_freq, Some(Freq::HfChannel(401)));
        assert_eq!(c[5].number.as_deref(), Some("0442079"));
        assert_eq!(c[6].position, Some(Position::Known { north: true, east: false, lat_deg: 40, lat_min: 30, lon_deg: 73, lon_min: 15 }));
        assert_eq!(c[6].time, Some(Utc::Known(9, 5)));
        assert_eq!(c[7].tc1, Some(118));
    }

    #[test]
    fn dsc_offset_clock_error_and_polarity() {
        for (k, (offset, ppm, invert)) in [(50.0f32, 100.0f32, false), (-50.0, -100.0, false), (95.0, 100.0, false), (-95.0, -100.0, true)].into_iter().enumerate() {
            let tx = Tx { offset, ppm, invert, snr: 0.0, ..Tx::default() };
            let (c, st) = one_call(&distress_alert(), &tx, 10 + k as u64);
            assert_eq!(c.len(), 1, "{offset} Hz {ppm} ppm: {c:#?}");
            check_alert(&c[0]);
            let center = st.center_hz.unwrap();
            assert!((center - (1700.0 + offset) * (1.0 + ppm * 1e-6)).abs() < 3.0, "center {center} for {offset}");
            assert_eq!(st.reverse, invert);
            assert_eq!(c[0].reverse, invert);
        }
    }

    /// What a call says, without how it was received.
    fn content_of(c: &DscCall) -> DscCall {
        DscCall { ecc_ok: false, rx_used: 0, unreadable: 0, center_hz: 0.0, snr_db: None, reverse: false, ..c.clone() }
    }

    /// Out of `n` seeds of `content`: the calls that passed their checks
    /// (ECC and fields) and said what was sent, and those that passed them
    /// and said something else.
    fn decoded(content: &[u8], tx: &Tx, n: u64, seed: u64) -> (u64, u64) {
        let dx = dx_sequence(content);
        let sent: Vec<Option<u8>> = dx[..dx.len() - 2].iter().map(|v| Some(*v)).collect();
        let want = content_of(&parse(&sent, Some(dx.len() - 4)));
        let (mut ok, mut wrong) = (0, 0);
        for s in 0..n {
            let (c, _) = one_call(content, tx, seed + s);
            if let Some(c) = c.iter().find(|c| c.complete && c.ecc_ok && c.bad_fields.is_empty()) {
                if content_of(c) == want {
                    ok += 1;
                } else {
                    println!("passed its checks but wrong: {}", c.summary());
                    wrong += 1;
                }
            }
        }
        (ok, wrong)
    }

    #[test]
    fn dsc_through_noise() {
        // In 2500 Hz: at -6 dB a bit has an Eb/N0 of 8 dB. The table test
        // measured every alert of 12 at -5 dB and 9 at -7 dB.
        for (snr, want) in [(-3.0f32, 4u64), (-5.0, 3), (-7.0, 2)] {
            let tx = Tx { snr, offset: 30.0, ppm: 50.0, ..Tx::default() };
            let (a, wa) = decoded(&distress_alert(), &tx, 4, 100);
            let (b, wb) = decoded(&individual_call(), &tx, 4, 200);
            let (c, wc) = decoded(&all_ships_safety(), &tx, 4, 300);
            println!("DSC at {snr:+.0} dB SNR (2500 Hz): alert {a}/4, individual {b}/4, all ships {c}/4");
            assert!(a >= want && b >= want && c >= want, "{snr} dB");
            assert_eq!(wa + wb + wc, 0);
        }
    }

    #[test]
    fn dsc_through_fading() {
        // Fades last longer than the 400 ms between a character's copies,
        // so they often take both: the table test measured 11 or 12 of 12
        // at 0 dB and 6 to 8 at -3 dB.
        for (ch, snr, want) in [(Channel::MODERATE, 0.0f32, 3u64), (Channel::POOR, 0.0, 3), (Channel::Selective(0.5), 0.0, 3)] {
            let tx = Tx { snr, ch, offset: -20.0, ppm: -80.0, ..Tx::default() };
            let (a, wa) = decoded(&distress_alert(), &tx, 4, 400);
            let (b, wb) = decoded(&individual_call(), &tx, 4, 500);
            let (c, wc) = decoded(&all_ships_safety(), &tx, 4, 600);
            println!("DSC {} at {snr:+.0} dB: alert {a}/4, individual {b}/4, all ships {c}/4", ch.name());
            assert!(a >= want && b >= want && c >= want, "{}", ch.name());
            assert_eq!(wa + wb + wc, 0);
        }
    }

    #[test]
    fn dsc_rx_copy_recovers_destroyed_dx() {
        let dx = dx_sequence(&distress_alert());
        let sl = slots(&dx);
        let mut lv = levels(&sl, 200);
        // Ruin the DX copy of five characters: self-ID, nature, position
        // and time, three bits each, so every one fails its check.
        let ruined = [3usize, 7, 9, 12, 14];
        for &d in &ruined {
            let at = 200 + 10 * (12 + 2 * d);
            for j in [0, 3, 5] {
                lv[at + j] = !lv[at + j];
            }
        }
        let (c, _) = run(&audio(&[lv], &Tx { snr: 10.0, ..Tx::default() }, 20));
        assert_eq!(c.len(), 1, "{c:#?}");
        check_alert(&c[0]);
        assert!(c[0].rx_used >= ruined.len() as u32, "rx copies {}", c[0].rx_used);
        assert_eq!(c[0].unreadable, 0);
    }

    #[test]
    fn dsc_rx_copy_covers_a_noise_burst() {
        let dx = dx_sequence(&individual_call());
        let lv = levels(&slots(&dx), 200);
        let tx = Tx { snr: 10.0, ..Tx::default() };
        let mut x = audio(&[lv], &tx, 21);
        // 350 ms of noise 20 dB over the signal in the middle of the call:
        // it takes in at least one whole DX slot, and the two copies of a
        // character, 500 ms apart, never both.
        let start = ((1.0 + 2.0 + 1.2 + 1.0) * FS) as usize;
        let sigma = noise_sigma(0.1, -20.0, FS);
        let mut rng = Rng::new(77);
        for v in &mut x[start..start + (0.35 * FS) as usize] {
            *v += sigma * rng.gauss();
        }
        let (c, _) = run(&x);
        assert_eq!(c.len(), 1, "{c:#?}");
        check_individual(&c[0]);
        assert!(c[0].rx_used >= 1, "rx copies {}", c[0].rx_used);
    }

    #[test]
    fn dsc_corrupted_call_fails_its_ecc() {
        let mut dx = dx_sequence(&individual_call());
        // Both copies of one self-ID character turn into another valid
        // character: the time diversity cannot see it, the ECC must.
        dx[10] ^= 0x03;
        let (c, _) = run(&audio(&[levels(&slots(&dx), 200)], &Tx { snr: 15.0, ..Tx::default() }, 30));
        assert_eq!(c.len(), 1, "{c:#?}");
        assert!(c[0].complete);
        assert!(!c[0].ecc_ok);
        assert!(c[0].summary().ends_with("ECC FAILED"), "{}", c[0].summary());
        assert!(c[0].fields().contains(&("ecc", "failed".into())));
    }

    #[test]
    fn dsc_cut_off_call_is_flagged() {
        let dx = dx_sequence(&distress_alert());
        let mut lv = levels(&slots(&dx), 200);
        // The signal goes after the time, before the closing sequence.
        lv.truncate(200 + 10 * (12 + 2 * 14));
        let (c, _) = run(&audio(&[lv], &Tx { snr: 15.0, ..Tx::default() }, 31));
        assert_eq!(c.len(), 1, "{c:#?}");
        assert!(!c[0].complete && !c[0].ecc_ok);
        assert_eq!(c[0].from.as_ref().unwrap().digits, SHIP);
        println!("{}", c[0].summary());
        assert!(c[0].summary().ends_with("INCOMPLETE"), "{}", c[0].summary());
        // What came before the cut is there; nothing after it is made up
        // from the noise that followed.
        let d = c[0].distress.as_ref().unwrap();
        assert_eq!((d.nature, d.position.is_some()), (Some(100), true));
        assert_eq!((d.time.clone(), d.subsequent), (None, None));
        assert!(c[0].bad_fields.is_empty());
    }

    #[test]
    fn dsc_back_to_back_alerts() {
        // A single-frequency distress attempt: five alerts with no gap.
        let lv = levels(&slots(&dx_sequence(&distress_alert())), 200);
        let runs = vec![lv; 5];
        let (c, _) = run(&audio(&runs, &Tx { snr: 5.0, ..Tx::default() }, 40));
        assert_eq!(c.len(), 5, "{c:#?}");
        c.iter().for_each(check_alert);
    }

    #[test]
    fn dsc_follows_a_new_station() {
        // Two calls from stations 60 Hz apart, a few seconds between.
        let a = levels(&slots(&dx_sequence(&individual_call())), 200);
        let b = levels(&slots(&dx_sequence(&all_ships_safety())), 20);
        let mut rng = Rng::new(50);
        let amp = 0.1;
        let mut x = vec![0.0; FS as usize];
        x.extend(fsk_through(&a, BAUD, FS, 1670.0, SHIFT_HZ, amp, Channel::Awgn, &mut rng));
        x.extend(vec![0.0; (4.0 * FS) as usize]);
        x.extend(fsk_through(&b, BAUD, FS, 1730.0, SHIFT_HZ, amp, Channel::Awgn, &mut rng));
        x.extend(vec![0.0; FS as usize]);
        add_noise(&mut x, amp, 5.0, FS, &mut rng);
        let (c, _) = run(&x);
        assert_eq!(c.len(), 2, "{c:#?}");
        check_individual(&c[0]);
        check_all_ships(&c[1]);
        assert!((c[0].center_hz - 1670.0).abs() < 4.0 && (c[1].center_hz - 1730.0).abs() < 4.0);
    }

    #[test]
    fn dsc_noise_and_other_signals_yield_nothing() {
        for (seed, amp) in [(61u64, 1e-4f32), (62, 0.05), (63, 0.8)] {
            let mut rng = Rng::new(seed);
            let x: Vec<f32> = (0..(60.0 * FS) as usize).map(|_| amp * rng.gauss()).collect();
            let (c, _) = run(&x);
            assert!(c.is_empty(), "{c:#?}");
        }
        // A DSC call on the routine channel 500 Hz up is not ours.
        let lv = levels(&slots(&dx_sequence(&individual_call())), 200);
        let mut rng = Rng::new(64);
        let mut x = vec![0.0; FS as usize];
        x.extend(fsk(&lv, BAUD, FS, 2200.0, SHIFT_HZ, 0.1));
        x.extend(vec![0.0; FS as usize]);
        add_noise(&mut x, 0.1, 10.0, FS, &mut rng);
        let (c, st) = run(&x);
        assert!(c.is_empty(), "{c:#?}");
        assert!(!st.locked);
    }

    #[test]
    fn dsc_threshold_table() {
        if std::env::var("USDR_CER_TABLE").is_err() {
            return;
        }
        let seeds: u64 = std::env::var("USDR_CER_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(8);
        let snrs = [10.0f32, 5.0, 0.0, -3.0, -5.0, -7.0, -9.0];
        println!("DSC: distress alerts decoded, of {seeds}, by SNR (2500 Hz); n+w! means w passed every check but were wrong");
        println!("{:>16} {}", "", snrs.iter().map(|s| format!("{s:>6.0}")).collect::<String>());
        for ch in [Channel::Awgn, Channel::MODERATE, Channel::POOR, Channel::Selective(0.5)] {
            let mut row = String::new();
            for &snr in &snrs {
                let tx = Tx { snr, ch, offset: 40.0, ppm: 100.0, ..Tx::default() };
                let (n, w) = decoded(&distress_alert(), &tx, seeds, 900);
                let _ = write!(row, "{:>6}", if w > 0 { format!("{n}+{w}!") } else { n.to_string() });
            }
            println!("{:>16} {row}", ch.name());
        }
    }

    #[test]
    fn dsc_realtime_factor() {
        let lv = levels(&slots(&dx_sequence(&individual_call())), 200);
        let x = audio(&[lv.clone(), lv.clone(), lv], &Tx { snr: -3.0, ..Tx::default() }, 70);
        let mut d = DscDecoder::new(FS);
        let t = std::time::Instant::now();
        chunks(&x, |c| d.push(c));
        let el = t.elapsed().as_secs_f64();
        let secs = x.len() as f64 / FS as f64;
        println!("DSC: {secs:.1} s of audio in {:.1} ms, {:.3}% of one core", el * 1e3, 100.0 * el / secs);
        assert!(el / secs < 0.05);
    }
}
