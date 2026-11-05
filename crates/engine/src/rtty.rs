//! RTTY: Baudot (ITA2) frequency-shift keying, and the FSK receiver that
//! NAVTEX shares.
//!
//! Sideband audio arrives at any level with the tone pair wherever the
//! operator happened to tune. The receiver ([`FskReceiver`]) works in three
//! steps:
//!
//! 1. The audio is cut to 12 kHz. Everything an FSK pair can occupy in a
//!    3 kHz passband survives, and every later stage runs at a quarter of the
//!    input rate.
//! 2. A 2048-point spectrum (5.9 Hz bins), averaged over about a second, is
//!    searched for two peaks `shift` apart. The weaker of the two tones has
//!    to stand above the median of the passband, so a lone carrier, a voice
//!    or plain noise never looks like a pair. The pair has to be found in
//!    three evaluations in a row before the receiver locks.
//! 3. Once locked, the pair's center is mixed to 0 Hz, filtered and
//!    decimated to about sixteen samples a bit, and each tone is mixed to DC
//!    and summed over exactly one bit: a boxcar matched filter for each tone.
//!
//! Matched filters rather than an FM discriminator: a discriminator has to
//! see both tones at once, so its noise bandwidth is the shift plus the
//! keying sidebands, two to four times what one tone's matched filter
//! admits, and below about 10 dB in that bandwidth it breaks into clicks
//! that read as wrong bits. Comparing the two tones' energies is the
//! optimum noncoherent FSK detector, degrades gradually instead, and the two
//! levels it produces are what selective-fading correction ([`Atc`]) and
//! the signal-presence gate need.
//!
//! The receiver keeps the last two seconds of audio and runs it through the
//! demodulator when it locks, so the characters sent while the pair was
//! still being found are not lost. While the pair is present, its center
//! follows drift from the phase advance of the stronger tone's filter.
//!
//! On HF the copy is lost to fading more than to noise, and the decoder is
//! built around that: each tone is weighed by its own level ([`Atc`]), so
//! a tone that has faded is not outvoted by the one that has not; machine
//! RTTY is framed on its character clock once found, so a lost start bit
//! costs one character rather than a run of misframed ones; and text
//! decoded while the squelch has shut on a fade is held and printed if it
//! opens again within seconds.

use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::collections::VecDeque;
use std::f32::consts::{PI, TAU};
use std::sync::Arc;

/// Keeps divisions by a silent channel finite.
const TINY: f32 = 1e-30;
/// The rate the receiver works at, before the per-mode decimation.
const WORK_RATE: f32 = 12_000.0;
/// Where a tone may sit in the sideband audio.
const TONE_LO_HZ: f32 = 100.0;
const TONE_HI_HZ: f32 = 3_400.0;
/// Averaging weight of each new spectrum: about a second at 85 ms a frame.
const SPEC_ALPHA: f32 = 0.1;
/// Frames averaged before a pair may be claimed. A young average is noisy
/// enough that a pair of noise bumps could pass.
const MIN_FRAMES: u32 = 8;
/// Mean excess of the weaker tone over the noise floor, in units of the
/// floor, to acquire and to hold a pair. Noise alone sits near zero with a
/// spread under 0.1 after averaging.
const ACQUIRE_RHO: f32 = 0.7;
const HOLD_RHO: f32 = 0.3;
/// Evaluations in a row that must find the same pair before locking.
const CONFIRM: u32 = 3;
/// A locked pair absent this long is given up and the search resumes.
const HOLD_S: f32 = 6.0;
/// Audio kept for the replay at lock.
const HISTORY_S: f32 = 2.0;
/// Bandwidth the reported SNR is referred to, as WSJT does.
const SNR_REF_HZ: f32 = 2_500.0;
/// Samples per bit the demodulator aims for: enough for edge timing to a
/// sixteenth of a bit without interpolating.
const TARGET_SPB: f32 = 16.0;
/// Seconds of tone phase averaged for each drift correction.
const AFC_S: f32 = 0.5;

// ---------------------------------------------------------------------------
// Small DSP pieces.

/// Blackman-windowed sinc lowpass with unity gain at DC; `cutoff` in cycles
/// per sample is the -6 dB point.
fn lowpass(ntaps: usize, cutoff: f32) -> Vec<f32> {
    let n = ntaps | 1;
    let mid = (n / 2) as f32;
    let m = (n - 1) as f32;
    let mut taps: Vec<f32> = (0..n)
        .map(|i| {
            let x = i as f32 - mid;
            let sinc = if x == 0.0 { 2.0 * cutoff } else { (TAU * cutoff * x).sin() / (PI * x) };
            let w = 0.42 - 0.5 * (TAU * i as f32 / m).cos() + 0.08 * (2.0 * TAU * i as f32 / m).cos();
            sinc * w
        })
        .collect();
    let sum: f32 = taps.iter().sum();
    taps.iter_mut().for_each(|t| *t /= sum);
    taps
}

/// Taps for a Blackman lowpass whose transition runs from `pass_hz` to
/// `stop_hz`: the window's transition is about 5.5 fs / N wide.
fn design(fs: f32, pass_hz: f32, stop_hz: f32, max_taps: usize) -> Vec<f32> {
    let half = (0.5 * (stop_hz - pass_hz)).max(1.0);
    let n = ((2.75 * fs / half).ceil() as usize).clamp(15, max_taps) | 1;
    lowpass(n, 0.5 * (pass_hz + stop_hz) / fs)
}

/// A complex oscillator stepping by repeated multiplication, renormalized
/// every 1024 samples, as `scannerd_dsp::Nco` does; this one is stepped a
/// sample at a time inside the receiver's loop.
#[derive(Clone, Copy)]
struct Osc {
    cur: Complex32,
    step: Complex32,
    n: u32,
}

impl Osc {
    /// Mixes down by `hz`.
    fn new(hz: f64, fs: f64) -> Self {
        let mut o = Self { cur: Complex32::new(1.0, 0.0), step: Complex32::new(1.0, 0.0), n: 0 };
        o.set(hz, fs);
        o
    }

    /// A new rate, keeping the phase.
    fn set(&mut self, hz: f64, fs: f64) {
        let w = -std::f64::consts::TAU * hz / fs;
        self.step = Complex32::new(w.cos() as f32, w.sin() as f32);
    }

    #[inline]
    fn next(&mut self) -> Complex32 {
        let v = self.cur;
        self.cur *= self.step;
        self.n += 1;
        if self.n >= 1024 {
            self.n = 0;
            self.cur *= 1.5 - 0.5 * self.cur.norm_sqr();
        }
        v
    }
}

/// Decimating FIR for real samples that only evaluates the outputs it keeps.
/// The history is written twice so the window is always one contiguous slice.
struct RealDecim {
    taps: Vec<f32>,
    buf: Vec<f32>,
    pos: usize,
    decim: usize,
    phase: usize,
}

impl RealDecim {
    fn new(fs: f32, decim: usize) -> Self {
        if decim <= 1 {
            return Self { taps: Vec::new(), buf: Vec::new(), pos: 0, decim: 1, phase: 0 };
        }
        let fs_out = fs / decim as f32;
        // Pass the tones; stop where the fold around fs_out / 2 would land
        // back on them.
        let pass = TONE_HI_HZ.min(0.4 * fs_out);
        let taps = design(fs, pass, fs_out - pass, 255);
        let n = taps.len();
        Self { taps, buf: vec![0.0; 2 * n], pos: 0, decim, phase: 0 }
    }

    #[inline]
    fn push(&mut self, x: f32) -> Option<f32> {
        if self.decim == 1 {
            return Some(x);
        }
        let n = self.taps.len();
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
        let mut acc = [0.0f32; 4];
        let mut tc = self.taps.chunks_exact(4);
        let mut wc = win.chunks_exact(4);
        for (t, w) in (&mut tc).zip(&mut wc) {
            for k in 0..4 {
                acc[k] += t[k] * w[k];
            }
        }
        let mut s = acc[0] + acc[1] + acc[2] + acc[3];
        for (t, w) in tc.remainder().iter().zip(wc.remainder()) {
            s += t * w;
        }
        Some(s)
    }
}

/// The same for complex samples with real taps.
struct CplxDecim {
    taps: Vec<f32>,
    buf: Vec<Complex32>,
    pos: usize,
    decim: usize,
    phase: usize,
}

impl CplxDecim {
    fn new(taps: Vec<f32>, decim: usize) -> Self {
        let n = taps.len();
        Self { taps, buf: vec![Complex32::new(0.0, 0.0); 2 * n], pos: 0, decim: decim.max(1), phase: 0 }
    }

    fn reset(&mut self) {
        self.buf.fill(Complex32::new(0.0, 0.0));
        self.pos = 0;
        self.phase = 0;
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
        self.phase += 1;
        if self.phase < self.decim {
            return None;
        }
        self.phase = 0;
        let win = &self.buf[self.pos..self.pos + n];
        let (mut re, mut im) = (0.0f32, 0.0f32);
        for (t, w) in self.taps.iter().zip(win) {
            re += t * w.re;
            im += t * w.im;
        }
        Some(Complex32::new(re, im))
    }
}

// ---------------------------------------------------------------------------
// Finding the pair.

/// Excess of the two tone windows, and of the gap between them, over the
/// noise floor at one center.
#[derive(Clone, Copy, Debug, Default)]
struct Pair {
    rho_hi: f32,
    rho_lo: f32,
    rho_mid: f32,
    /// Power above the floor in both windows, in spectrum units.
    power: f32,
}

/// The weaker tone may be this far below the stronger, in power: an SSB
/// filter's edge or a selective fade tilts a pair, but a lone carrier's
/// skirt is far weaker than the carrier.
const MAX_TILT: f32 = 0.03;

impl Pair {
    /// How well this looks like a pair: the excess of its weaker tone, or
    /// zero when it is not shaped like one. A keyed pair has a gap between
    /// its tones; a single strong carrier sitting in the gap, with its
    /// skirts or key clicks reaching both windows, does not.
    fn score(&self) -> f32 {
        let weak = self.rho_hi.min(self.rho_lo);
        let strong = self.rho_hi.max(self.rho_lo);
        if weak < MAX_TILT * strong || self.rho_mid > weak {
            return 0.0;
        }
        weak
    }
}

/// The averaged spectrum and the pair search over it.
struct PairFinder {
    n: usize,
    hop: usize,
    bin_hz: f32,
    fft: Arc<dyn Fft<f32>>,
    win: Vec<f32>,
    ring: Vec<f32>,
    pos: usize,
    filled: usize,
    since: usize,
    work: Vec<Complex32>,
    scratch: Vec<Complex32>,
    avg: Vec<f32>,
    frames: u32,
    prefix: Vec<f64>,
    sorted: Vec<f32>,
    lo: usize,
    hi: usize,
    /// Half the shift and half a tone window, in bins.
    half_shift: f32,
    half_win: f32,
    floor: f32,
}

impl PairFinder {
    fn new(fs: f32, baud: f32, shift: f32) -> Self {
        let n = ((fs * 0.17) as usize).next_power_of_two().max(256);
        let bin_hz = fs / n as f32;
        let fft = FftPlanner::new().plan_fft_forward(n);
        let scratch = vec![Complex32::new(0.0, 0.0); fft.get_inplace_scratch_len()];
        let win = (0..n).map(|i| 0.5 - 0.5 * (TAU * i as f32 / n as f32).cos()).collect();
        let lo = (TONE_LO_HZ / bin_hz).ceil() as usize;
        let hi = ((TONE_HI_HZ.min(0.45 * fs)) / bin_hz).floor() as usize;
        // A window takes in a tone's keying sidebands but stops short of
        // the other tone's, so a single carrier can never fill both.
        let half_win = ((0.75 * baud).min(0.4 * shift) / bin_hz).max(1.5);
        Self {
            n,
            hop: n / 2,
            bin_hz,
            fft,
            win,
            ring: vec![0.0; n],
            pos: 0,
            filled: 0,
            since: 0,
            work: vec![Complex32::new(0.0, 0.0); n],
            scratch,
            avg: vec![0.0; n / 2],
            frames: 0,
            prefix: vec![0.0; n / 2 + 1],
            sorted: Vec::with_capacity(n / 2),
            lo,
            hi: hi.max(lo + 4).min(n / 2 - 1),
            half_shift: 0.5 * shift / bin_hz,
            half_win,
            floor: 0.0,
        }
    }

    /// One sample in; true when a new averaged spectrum is ready.
    fn push(&mut self, x: f32) -> bool {
        self.ring[self.pos] = x;
        self.pos = (self.pos + 1) % self.n;
        self.filled = (self.filled + 1).min(self.n);
        self.since += 1;
        if self.since < self.hop || self.filled < self.n {
            return false;
        }
        self.since = 0;
        self.frame();
        true
    }

    fn frame(&mut self) {
        let n = self.n;
        for (i, w) in self.work.iter_mut().enumerate() {
            *w = Complex32::new(self.ring[(self.pos + i) % n] * self.win[i], 0.0);
        }
        self.fft.process_with_scratch(&mut self.work, &mut self.scratch);
        self.frames += 1;
        let a = (1.0 / self.frames as f32).max(SPEC_ALPHA);
        for (avg, w) in self.avg.iter_mut().zip(&self.work) {
            *avg += a * (w.norm_sqr() - *avg);
        }
        let mut run = 0.0f64;
        self.prefix[0] = 0.0;
        for (k, &p) in self.avg.iter().enumerate() {
            run += p as f64;
            self.prefix[k + 1] = run;
        }
        // The median of the search range is the floor: signals occupy a
        // small share of the passband, so it lands on noise.
        self.sorted.clear();
        self.sorted.extend_from_slice(&self.avg[self.lo..=self.hi]);
        let mid = self.sorted.len() / 2;
        let (_, m, _) = self.sorted.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
        self.floor = *m;
    }

    fn window(&self, center_bin: f32) -> (usize, usize) {
        let last = self.avg.len() - 1;
        let a = ((center_bin - self.half_win).round().max(0.0) as usize).min(last);
        let b = ((center_bin + self.half_win).round().max(0.0) as usize).min(last);
        (a, b.max(a))
    }

    fn excess(&self, (a, b): (usize, usize)) -> (f32, f32) {
        let n = (b - a + 1) as f64;
        let floor = self.floor as f64;
        let e = self.prefix[b + 1] - self.prefix[a] - n * floor;
        ((e / (n * floor)) as f32, e as f32)
    }

    fn pair_at(&self, center_hz: f32) -> Pair {
        if self.floor <= 0.0 || !self.floor.is_finite() {
            return Pair::default();
        }
        let c = center_hz / self.bin_hz;
        let (rho_hi, p_hi) = self.excess(self.window(c + self.half_shift));
        let (rho_lo, p_lo) = self.excess(self.window(c - self.half_shift));
        let gap = (self.half_shift - self.half_win).max(0.5);
        let mid = ((c - gap).round().max(0.0) as usize, (c + gap).round().max(0.0) as usize);
        let (rho_mid, _) = self.excess((mid.0.min(self.avg.len() - 1), mid.1.clamp(mid.0, self.avg.len() - 1)));
        Pair { rho_hi, rho_lo, rho_mid, power: p_hi.max(0.0) + p_lo.max(0.0) }
    }

    /// Noise-subtracted centroid of a window, in bins.
    fn centroid(&self, (a, b): (usize, usize)) -> Option<f32> {
        let (mut sw, mut swk) = (0.0f32, 0.0f32);
        for k in a..=b {
            let p = (self.avg[k] - self.floor).max(0.0);
            sw += p;
            swk += p * k as f32;
        }
        (sw > 0.0).then(|| swk / sw)
    }

    /// The strongest pair in the passband: its center in Hz and the excess
    /// of its weaker tone.
    fn best(&self) -> Option<(f32, f32)> {
        if self.floor <= 0.0 || !self.floor.is_finite() {
            return None;
        }
        let reach = self.half_shift + self.half_win;
        let first = (self.lo as f32 + reach).ceil() as usize;
        let last = (self.hi as f32 - reach).floor() as usize;
        let mut best: Option<(usize, f32)> = None;
        for cb in first..=last.max(first) {
            let s = self.pair_at(cb as f32 * self.bin_hz).score();
            if best.is_none_or(|(_, b)| s > b) {
                best = Some((cb, s));
            }
        }
        let (cb, score) = best?;
        let c = cb as f32;
        let center = match (
            self.centroid(self.window(c + self.half_shift)),
            self.centroid(self.window(c - self.half_shift)),
        ) {
            (Some(h), Some(l)) => 0.5 * (h + l),
            _ => c,
        };
        Some((center * self.bin_hz, score))
    }

    fn snr_db(&self, p: &Pair) -> f32 {
        let noise = self.floor * SNR_REF_HZ / self.bin_hz;
        10.0 * (p.power.max(TINY) / noise.max(TINY)).log10()
    }
}

// ---------------------------------------------------------------------------
// The demodulator.

/// Mixes a tone pair's center to 0 Hz, decimates to about sixteen samples a
/// bit, and gives the amplitude of each tone's one-bit matched filter.
pub struct FskDemod {
    fs1: f32,
    fs2: f32,
    baud: f32,
    center: f32,
    osc: Osc,
    chan: CplxDecim,
    osc_hi: Osc,
    osc_lo: Osc,
    ring_hi: Vec<Complex32>,
    ring_lo: Vec<Complex32>,
    rpos: usize,
    sum_hi: Complex32,
    sum_lo: Complex32,
    /// Outputs since the running sums were last rebuilt from the rings, so
    /// rounding in the add-and-subtract cannot build up.
    since_resum: u32,
    prev_hi: Complex32,
    prev_lo: Complex32,
    afc_acc: Complex32,
    afc_n: usize,
    afc_len: usize,
}

impl FskDemod {
    /// `fs1` is the rate of the audio given to [`FskDemod::push`].
    pub fn new(fs1: f32, baud: f32, shift_hz: f32, center_hz: f32) -> Self {
        let baud = baud.max(1.0);
        let shift = shift_hz.max(1.0);
        // The decimated band has to hold both tones and their first
        // sidebands with room for a filter edge.
        let pass = 0.5 * shift + baud;
        let want = (TARGET_SPB * baud).max(2.0 * pass + 4.0 * baud);
        let decim = ((fs1 / want).floor() as usize).max(1);
        let fs2 = fs1 / decim as f32;
        let stop = (fs2 - pass).max(pass + 0.1 * fs2);
        let taps = design(fs1, pass, stop, 511);
        let len = ((fs2 / baud).round() as usize).max(2);
        let mut d = Self {
            fs1,
            fs2,
            baud,
            center: center_hz,
            osc: Osc::new(center_hz as f64, fs1 as f64),
            chan: CplxDecim::new(taps, decim),
            osc_hi: Osc::new(0.5 * shift as f64, fs2 as f64),
            osc_lo: Osc::new(-0.5 * shift as f64, fs2 as f64),
            ring_hi: vec![Complex32::new(0.0, 0.0); len],
            ring_lo: vec![Complex32::new(0.0, 0.0); len],
            rpos: 0,
            sum_hi: Complex32::new(0.0, 0.0),
            sum_lo: Complex32::new(0.0, 0.0),
            since_resum: 0,
            prev_hi: Complex32::new(0.0, 0.0),
            prev_lo: Complex32::new(0.0, 0.0),
            afc_acc: Complex32::new(0.0, 0.0),
            afc_n: 0,
            afc_len: ((AFC_S * fs2) as usize).max(1),
        };
        d.reset(center_hz);
        d
    }

    /// Output rate.
    pub fn rate(&self) -> f32 {
        self.fs2
    }

    pub fn samples_per_bit(&self) -> f32 {
        self.fs2 / self.baud
    }

    /// Length of the matched filter in output samples.
    pub fn filter_len(&self) -> usize {
        self.ring_hi.len()
    }

    pub fn center_hz(&self) -> f32 {
        self.center
    }

    /// Move the pair's center, keeping the filters' state: a drift
    /// correction should not disturb the bit in progress.
    pub fn set_center(&mut self, hz: f32) {
        self.center = hz;
        self.osc.set(hz as f64, self.fs1 as f64);
    }

    /// Start over at a new center.
    pub fn reset(&mut self, hz: f32) {
        self.set_center(hz);
        self.chan.reset();
        self.ring_hi.fill(Complex32::new(0.0, 0.0));
        self.ring_lo.fill(Complex32::new(0.0, 0.0));
        self.rpos = 0;
        self.sum_hi = Complex32::new(0.0, 0.0);
        self.sum_lo = Complex32::new(0.0, 0.0);
        self.prev_hi = self.sum_hi;
        self.prev_lo = self.sum_lo;
        self.afc_acc = Complex32::new(0.0, 0.0);
        self.afc_n = 0;
    }

    /// One sample at `fs1`; the upper and lower tone amplitudes when an
    /// output sample is due.
    #[inline]
    pub fn push(&mut self, x: f32) -> Option<(f32, f32)> {
        let y = self.chan.push(self.osc.next() * x)?;
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
            self.since_resum = 0;
            self.sum_hi = self.ring_hi.iter().sum();
            self.sum_lo = self.ring_lo.iter().sum();
        }
        let inv = 1.0 / self.ring_hi.len() as f32;
        let ah = self.sum_hi.norm() * inv;
        let al = self.sum_lo.norm() * inv;
        // The stronger tone's filter turns at the tone's offset from where
        // it is expected; the weaker one turns at whatever leaks in.
        if ah > al {
            self.afc_acc += self.sum_hi * self.prev_hi.conj();
        } else {
            self.afc_acc += self.sum_lo * self.prev_lo.conj();
        }
        self.prev_hi = self.sum_hi;
        self.prev_lo = self.sum_lo;
        self.afc_n += 1;
        Some((ah, al))
    }

    /// The tones' frequency error in Hz, positive when they sit above where
    /// they are expected, once every half second of output.
    pub fn take_freq_error(&mut self) -> Option<f32> {
        if self.afc_n < self.afc_len {
            return None;
        }
        let a = std::mem::replace(&mut self.afc_acc, Complex32::new(0.0, 0.0));
        self.afc_n = 0;
        (a.norm_sqr() > 0.0).then(|| a.arg() * self.fs2 / TAU)
    }
}

// ---------------------------------------------------------------------------
// The receiver.

/// One demodulator output sample: each tone's matched-filter amplitude.
#[derive(Clone, Copy, Debug, Default)]
pub struct FskSample {
    /// The upper audio tone.
    pub hi: f32,
    /// The lower audio tone.
    pub lo: f32,
    /// The first sample after a lock: whatever the caller was framing
    /// belongs to another signal or none.
    pub fresh: bool,
}

/// Finds an FSK pair in sideband audio, locks on it, follows its drift and
/// demodulates it. Shared by the RTTY and NAVTEX decoders.
pub struct FskReceiver {
    baud: f32,
    front: RealDecim,
    finder: PairFinder,
    demod: FskDemod,
    fixed: Option<f32>,
    center: f32,
    running: bool,
    present: bool,
    absent: u32,
    hold_frames: u32,
    cand: Option<f32>,
    cand_n: u32,
    tol: f32,
    snr_db: f32,
    tracking: bool,
    fresh: bool,
    hist: Vec<f32>,
    hist_pos: usize,
    hist_len: usize,
}

impl FskReceiver {
    /// `center_hz`, when given, fixes the pair's center: no search and no
    /// drift tracking.
    pub fn new(fs: f32, baud: f32, shift_hz: f32, center_hz: Option<f32>) -> Self {
        let decim = if fs >= 1.5 * WORK_RATE { (fs / WORK_RATE).round() as usize } else { 1 };
        let fs1 = fs / decim as f32;
        let finder = PairFinder::new(fs1, baud, shift_hz);
        let hold_frames = (HOLD_S * fs1 / finder.hop as f32) as u32;
        let tol = (0.5 * baud).max(2.0 * finder.bin_hz);
        let start = center_hz.unwrap_or(1_500.0);
        let cap = (HISTORY_S * fs1) as usize;
        Self {
            baud,
            front: RealDecim::new(fs, decim),
            finder,
            demod: FskDemod::new(fs1, baud, shift_hz, start),
            fixed: center_hz,
            center: start,
            running: center_hz.is_some(),
            present: false,
            absent: 0,
            hold_frames,
            cand: None,
            cand_n: 0,
            tol,
            snr_db: f32::NEG_INFINITY,
            tracking: false,
            fresh: true,
            hist: vec![0.0; cap.max(1)],
            hist_pos: 0,
            hist_len: 0,
        }
    }

    /// Whether drift corrections may be applied. A decoder clears it while
    /// what it hears does not frame, so noise cannot walk the center away.
    pub fn set_tracking(&mut self, on: bool) {
        self.tracking = on;
    }

    /// Locked on a pair that is still there.
    pub fn locked(&self) -> bool {
        self.running && self.present
    }

    /// The averaged spectrum still shows the pair at the center.
    pub fn present(&self) -> bool {
        self.present
    }

    pub fn center_hz(&self) -> f32 {
        self.center
    }

    /// SNR of the pair in 2500 Hz, from the averaged spectrum.
    pub fn snr_db(&self) -> f32 {
        self.snr_db
    }

    /// Rate of the samples [`FskReceiver::push`] produces.
    pub fn rate(&self) -> f32 {
        self.demod.rate()
    }

    pub fn samples_per_bit(&self) -> f32 {
        self.demod.samples_per_bit()
    }

    /// Length of each tone's matched filter, in output samples.
    pub fn filter_len(&self) -> usize {
        self.demod.filter_len()
    }

    /// Audio in; demodulated samples out (`out` is cleared first). Nothing
    /// comes out until a pair is locked.
    pub fn push(&mut self, audio: &[f32], out: &mut Vec<FskSample>) {
        out.clear();
        let cap = self.hist.len();
        for &x in audio {
            let Some(y) = self.front.push(x) else { continue };
            self.hist[self.hist_pos] = y;
            self.hist_pos = (self.hist_pos + 1) % cap;
            self.hist_len = (self.hist_len + 1).min(cap);
            if self.finder.push(y) && self.evaluate() {
                // Newly locked: run what was heard while searching, this
                // sample included.
                self.demod.reset(self.center);
                self.fresh = true;
                for i in 0..self.hist_len {
                    let y = self.hist[(self.hist_pos + cap - self.hist_len + i) % cap];
                    self.step(y, out);
                }
                continue;
            }
            if self.running {
                self.step(y, out);
            }
        }
    }

    #[inline]
    fn step(&mut self, y: f32, out: &mut Vec<FskSample>) {
        let Some((hi, lo)) = self.demod.push(y) else { return };
        out.push(FskSample { hi, lo, fresh: self.fresh });
        self.fresh = false;
        if let Some(df) = self.demod.take_freq_error()
            && self.tracking
            && self.present
            && self.fixed.is_none()
        {
            // Half the measured error per half second follows drift within
            // a couple of seconds while averaging out noise.
            let max_step = (0.05 * self.baud).max(0.5);
            self.center += (0.5 * df).clamp(-max_step, max_step);
            self.demod.set_center(self.center);
        }
    }

    /// Look at a new spectrum. True when it completes a lock.
    fn evaluate(&mut self) -> bool {
        if self.finder.frames < MIN_FRAMES {
            return false;
        }
        if self.running {
            let p = self.finder.pair_at(self.center);
            self.present = p.score() > HOLD_RHO;
            if self.present {
                self.absent = 0;
                self.snr_db = self.finder.snr_db(&p);
            } else if self.fixed.is_none() {
                self.absent += 1;
                if self.absent > self.hold_frames {
                    self.running = false;
                    self.cand = None;
                    self.cand_n = 0;
                }
            }
            return false;
        }
        match self.finder.best() {
            Some((c, score)) if score > ACQUIRE_RHO => {
                self.cand_n = match self.cand {
                    Some(prev) if (prev - c).abs() <= self.tol => self.cand_n + 1,
                    _ => 1,
                };
                self.cand = Some(c);
                if self.cand_n >= CONFIRM {
                    self.center = c;
                    self.running = true;
                    self.present = true;
                    self.absent = 0;
                    self.snr_db = self.finder.snr_db(&self.finder.pair_at(c));
                    return true;
                }
            }
            _ => {
                self.cand = None;
                self.cand_n = 0;
            }
        }
        false
    }
}

// ---------------------------------------------------------------------------
// Selective-fading correction.

/// Automatic threshold correction, as a log-likelihood ratio.
///
/// On HF the two tones fade independently; one can drop 10 or 20 dB while
/// the other holds. Comparing raw levels then slices bits toward the strong
/// tone, and puts the zero crossings a bit edge's timing comes from off
/// center. With each tone's on-amplitude `A` and the noise known, the
/// likelihood ratio of noncoherent FSK at good SNR is
/// `A_hi (hi - A_hi/2) - A_lo (lo - A_lo/2)` over the noise power: a tone
/// that has faded away is compared with half its own level instead of with
/// the other tone, and through a transition the ratio crosses zero halfway
/// whatever the two levels are. It is the "optimal ATC" of Kok Chen
/// (W7AY). The levels are followed sample by sample, each tone's from the
/// samples where it is the stronger and above half its level, within a
/// couple of bits, so a fade is followed as it happens rather than learned
/// from characters that may not frame in it.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Atc {
    a_hi: f32,
    a_lo: f32,
    noise: f32,
    k_on: f32,
    k_noise: f32,
}

impl Atc {
    /// `spb` output samples to a bit.
    pub(crate) fn new(spb: f32) -> Self {
        Self { k_on: 1.0 / (2.0 * spb), k_noise: 1.0 / (8.0 * spb), ..Self::default() }
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::new(0.5 / self.k_on);
    }

    /// One sample: the soft decision, positive for the upper tone, scaled
    /// to about ±1 at a bit center.
    #[inline]
    pub(crate) fn step(&mut self, hi: f32, lo: f32) -> f32 {
        // A sample counts toward a tone's level when that tone is the
        // stronger and plainly on: above half its level, or well clear of
        // the other tone. The second lets a level follow a fade deeper
        // than 6 dB; without the first, the noise in a tone that has faded
        // out would drag the other tone's level down whenever it won.
        if hi > lo {
            if hi > 0.5 * self.a_hi || hi > 2.5 * lo {
                self.a_hi += self.k_on * (hi - self.a_hi);
            }
        } else if lo > 0.5 * self.a_lo || lo > 2.5 * hi {
            self.a_lo += self.k_on * (lo - self.a_lo);
        }
        let m = hi.min(lo);
        self.noise += self.k_noise * (m * m - self.noise);
        let (ah, al) = (self.a_hi, self.a_lo);
        let den = 0.5 * (ah * ah + al * al);
        if den <= TINY {
            return (hi - lo) / (hi + lo + TINY);
        }
        ((ah * (hi - 0.5 * ah) - al * (lo - 0.5 * al)) / den).clamp(-2.0, 2.0)
    }

    /// What [`Atc::step`]'s value is multiplied by to make it a
    /// log-likelihood ratio: the signal-to-noise ratio of a bit sample.
    pub(crate) fn llr_scale(&self) -> f32 {
        0.5 * (self.a_hi * self.a_hi + self.a_lo * self.a_lo) / (0.5 * self.noise).max(TINY)
    }
}

// ---------------------------------------------------------------------------
// The code tables.

/// One row of ITU-R M.476 Table 1: the ITA2 code, the CCIR 476 code, and
/// the letter- and figure-case meanings. Codes are stored with the first
/// transmitted bit as the least significant, so a shift register fed
/// oldest-first reads them directly.
#[derive(Clone, Copy)]
pub(crate) struct Sym {
    pub ita2: u8,
    pub ccir: u8,
    pub letter: char,
    pub figure: char,
}

/// Letter- and figure-case shift codes, returned as the SI and SO controls.
pub(crate) const LTRS: char = '\u{0f}';
pub(crate) const FIGS: char = '\u{0e}';

/// Bits in transmission order, `one` marking the mark (Z, B) state.
const fn bits(p: &[u8], one: u8) -> u8 {
    let mut v = 0u8;
    let mut i = 0;
    while i < p.len() {
        if p[i] == one {
            v |= 1 << i;
        }
        i += 1;
    }
    v
}

const fn sym(ita2: &[u8], ccir: &[u8], letter: char, figure: char) -> Sym {
    Sym { ita2: bits(ita2, b'Z'), ccir: bits(ccir, b'B'), letter, figure }
}

/// ITU-R M.476-5 Table 1, international figure case. The figure cases of
/// F, G and H are unassigned; D's is "who are you" and J's the bell, which
/// print nothing.
pub(crate) const SYMS: [Sym; 32] = [
    sym(b"ZZAAA", b"BBBYYYB", 'A', '-'),
    sym(b"ZAAZZ", b"YBYYBBB", 'B', '?'),
    sym(b"AZZZA", b"BYBBBYY", 'C', ':'),
    sym(b"ZAAZA", b"BBYYBYB", 'D', '\0'),
    sym(b"ZAAAA", b"YBBYBYB", 'E', '3'),
    sym(b"ZAZZA", b"BBYBBYY", 'F', '\0'),
    sym(b"AZAZZ", b"BYBYBBY", 'G', '\0'),
    sym(b"AAZAZ", b"BYYBYBB", 'H', '\0'),
    sym(b"AZZAA", b"BYBBYYB", 'I', '8'),
    sym(b"ZZAZA", b"BBBYBYY", 'J', '\0'),
    sym(b"ZZZZA", b"YBBBBYY", 'K', '('),
    sym(b"AZAAZ", b"BYBYYBB", 'L', ')'),
    sym(b"AAZZZ", b"BYYBBBY", 'M', '.'),
    sym(b"AAZZA", b"BYYBBYB", 'N', ','),
    sym(b"AAAZZ", b"BYYYBBB", 'O', '9'),
    sym(b"AZZAZ", b"BYBBYBY", 'P', '0'),
    sym(b"ZZZAZ", b"YBBBYBY", 'Q', '1'),
    sym(b"AZAZA", b"BYBYBYB", 'R', '4'),
    sym(b"ZAZAA", b"BBYBYYB", 'S', '\''),
    sym(b"AAAAZ", b"YYBYBBB", 'T', '5'),
    sym(b"ZZZAA", b"YBBBYYB", 'U', '7'),
    sym(b"AZZZZ", b"YYBBBBY", 'V', '='),
    sym(b"ZZAAZ", b"BBBYYBY", 'W', '2'),
    sym(b"ZAZZZ", b"YBYBBBY", 'X', '/'),
    sym(b"ZAZAZ", b"BBYBYBY", 'Y', '6'),
    sym(b"ZAAAZ", b"BBYYYBB", 'Z', '+'),
    sym(b"AAAZA", b"YYYBBBB", '\r', '\r'),
    sym(b"AZAAA", b"YYBBYBB", '\n', '\n'),
    sym(b"ZZZZZ", b"YBYBBYB", LTRS, LTRS),
    sym(b"ZZAZZ", b"YBBYBBY", FIGS, FIGS),
    sym(b"AAZAA", b"YYBBBYB", ' ', ' '),
    sym(b"AAAAA", b"YBYBYBB", '\0', '\0'),
];

/// Figure cases where US teleprinters, and so most amateur stations,
/// differ from the international alphabet.
fn us_figure(letter: char) -> Option<char> {
    Some(match letter {
        'D' => '$',
        'F' => '!',
        'G' => '&',
        'H' => '#',
        'J' => '\'',
        'S' => '\u{07}',
        'V' => ';',
        'Z' => '"',
        _ => return None,
    })
}

const fn ita2_index() -> [u8; 32] {
    let mut t = [0u8; 32];
    let mut i = 0;
    while i < SYMS.len() {
        t[SYMS[i].ita2 as usize] = i as u8;
        i += 1;
    }
    t
}

/// Row of [`SYMS`] for each ITA2 code.
pub(crate) const ITA2_INDEX: [u8; 32] = ita2_index();

/// Letter/figure shift state and the code-to-character step both decoders use.
#[derive(Clone, Debug, Default)]
pub(crate) struct Shift {
    pub figs: bool,
    /// Rows received since a figure shift, held until the word they make
    /// shows whether that shift was real; see [`Shift::feed`].
    pending: Vec<u8>,
    guard: bool,
}

/// Characters held after a figure shift before it is judged.
const GUARD_MAX: usize = 8;

/// Whether a word read in figures is plausible as figures. Real figure
/// runs are numbers: digits, with the odd point, comma, dash, slash or
/// colon among them, or a lone mark of punctuation ("SEA:", "(OK)"). A
/// letter shift misread as a figure shift (they differ in one bit) turns
/// the next word of letters into punctuation-heavy figures instead: CQ
/// becomes ":1", DE "$3", THE "5#3". Such a word is taken as letters.
fn plausible_figures(t: &str) -> bool {
    let n = t.chars().count();
    if n <= 1 {
        return true;
    }
    let digits = t.chars().filter(char::is_ascii_digit).count();
    let first = t.chars().next().unwrap_or(' ');
    let opens = first.is_ascii_digit() || matches!(first, '(' | '-' | '+' | '.' | '/');
    opens && n - digits <= digits
}

impl Shift {
    /// One row: the characters it completes go to `out`.
    ///
    /// After a figure shift from letters, rows are held to the end of the
    /// word (a space, line end or shift, or eight characters) and the word
    /// is printed in figures only if it reads plausibly as figures; if not,
    /// the shift is taken for a misread letter shift and the word, and what
    /// follows, print as letters. Without this one bad bit at the end of
    /// DWD's RY run printed "CQ CQ CQ" as ":1 :1 :1", and with no unshift on
    /// space nothing recovered before the next letter shift.
    pub(crate) fn feed(&mut self, row: usize, usos: bool, us: bool, out: &mut Vec<char>) {
        let s = &SYMS[row];
        if self.guard {
            let ends = matches!(s.letter, LTRS | FIGS | ' ' | '\r' | '\n');
            if !ends && self.pending.len() < GUARD_MAX {
                self.pending.push(row as u8);
                return;
            }
            self.flush(usos, us, out);
        }
        let was = self.figs;
        if let Some(c) = self.apply(s, usos, us) {
            out.push(c);
        }
        self.guard = !was && self.figs && s.letter == FIGS;
    }

    /// Judge and let go of whatever is held.
    pub(crate) fn flush(&mut self, usos: bool, us: bool, out: &mut Vec<char>) {
        self.guard = false;
        if self.pending.is_empty() {
            return;
        }
        let fig = |r: u8| {
            let s = &SYMS[r as usize];
            if us { us_figure(s.letter).unwrap_or(s.figure) } else { s.figure }
        };
        let t: String = self.pending.iter().map(|&r| fig(r)).collect();
        if !t.chars().all(|c| c.is_ascii_graphic()) || !plausible_figures(&t) {
            self.figs = false;
        }
        for r in std::mem::take(&mut self.pending) {
            if let Some(c) = self.apply(&SYMS[r as usize], usos, us) {
                out.push(c);
            }
        }
    }

    /// The printable character (or CR/LF) a row stands for, updating the
    /// shift state. `usos` returns to letters on a space; `us` selects the
    /// US figure case.
    pub(crate) fn apply(&mut self, s: &Sym, usos: bool, us: bool) -> Option<char> {
        let mut c = if self.figs {
            if us { us_figure(s.letter).unwrap_or(s.figure) } else { s.figure }
        } else {
            s.letter
        };
        // A letter with no figure case (F, G and H internationally) or with
        // a control there (the bell, "who are you") is far more likely sent
        // in letters after a letter shift lost in a fade than sent in
        // figures. Taking it as a letter ends a run of figures that would
        // otherwise garble everything up to the next shift.
        if self.figs && s.letter.is_ascii_uppercase() && !c.is_ascii_graphic() {
            self.figs = false;
            c = s.letter;
        }
        match c {
            LTRS => {
                self.figs = false;
                None
            }
            FIGS => {
                self.figs = true;
                None
            }
            ' ' => {
                if usos {
                    self.figs = false;
                }
                Some(' ')
            }
            '\r' | '\n' => Some(c),
            c if c.is_ascii_graphic() => Some(c),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Line chunking, shared with NAVTEX.

/// Lines longer than this are cut.
pub(crate) const LINE_MAX: usize = 80;
/// Pending text is let go after this long without a new character.
pub(crate) const IDLE_FLUSH_S: f32 = 2.0;

/// Collects characters into lines.
#[derive(Default)]
pub(crate) struct Lines {
    line: String,
    len: usize,
}

impl Lines {
    /// One character; a finished line when it ends one.
    pub(crate) fn push(&mut self, c: char) -> Option<String> {
        if c == '\r' || c == '\n' {
            return self.take();
        }
        self.line.push(c);
        self.len += 1;
        if self.len >= LINE_MAX { self.take() } else { None }
    }

    /// Whatever is pending, without trailing blanks; None when blank.
    pub(crate) fn take(&mut self) -> Option<String> {
        self.len = 0;
        let s = std::mem::take(&mut self.line);
        let t = s.trim_end();
        (!t.is_empty()).then(|| t.to_string())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }
}

// ---------------------------------------------------------------------------
// RTTY.

#[derive(Clone, Debug, PartialEq)]
pub struct RttyConfig {
    pub baud: f32,
    pub shift_hz: f32,
    /// Stop bits the station sends. The framer accepts any stop of at
    /// least one bit, so this does not have to be exact.
    pub stop_bits: f32,
    /// A fixed pair center in audio Hz; `None` searches for the pair.
    pub center_hz: Option<f32>,
    /// Force the polarity: `Some(true)` puts mark on the lower audio tone.
    /// `None` detects it from framing.
    pub reverse: Option<bool>,
    /// Return to letters on a space.
    pub unshift_on_space: bool,
}

impl RttyConfig {
    /// Amateur RTTY: 45.45 baud, 170 Hz shift.
    pub fn amateur() -> Self {
        Self { baud: 45.45, shift_hz: 170.0, stop_bits: 1.5, center_hz: None, reverse: None, unshift_on_space: true }
    }

    /// DWD's weather broadcasts: 50 baud, 450 Hz shift. DWD's
    /// teleprinters do not unshift on a space: "SEA: 0,5 METER" is sent
    /// with one figure shift before the colon, and unshifting at the space
    /// would print the 0,5 as PNT.
    pub fn dwd() -> Self {
        Self { baud: 50.0, shift_hz: 450.0, unshift_on_space: false, ..Self::amateur() }
    }

    /// 75 baud, 850 Hz shift.
    pub fn wide() -> Self {
        Self { baud: 75.0, shift_hz: 850.0, ..Self::amateur() }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RttyText {
    pub text: String,
    pub center_hz: f32,
    pub snr_db: f32,
    pub reverse: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RttyStatus {
    /// Locked on a pair that is still there.
    pub locked: bool,
    /// Characters are being printed: the pair frames cleanly.
    pub printing: bool,
    pub center_hz: f32,
    pub reverse: bool,
    pub snr_db: f32,
    /// Characters printed since the decoder was made.
    pub chars: u64,
    /// Share of recent characters with a good stop bit, 0..1.
    pub frame_quality: f32,
}

/// Frame-quality averaging length, in characters.
const Q_LEN: f32 = 8.0;
/// Squelch thresholds: frame quality, and the on/off tone power ratio at
/// the sampling instants. Noise alone frames about half its characters
/// and shows a ratio near 3.
const OPEN_Q: f32 = 0.7;
const CLOSE_Q: f32 = 0.5;
const OPEN_RATIO: f32 = 5.0;
const CLOSE_RATIO: f32 = 3.0;
/// Characters held back while the squelch is closed, printed if it opens.
const PREROLL: usize = 64;
/// A squelch that shut this recently, on a pair still present, shut on a
/// fade: everything decoded meanwhile is held and printed if it reopens.
const FADE_HOLD_S: f32 = 10.0;

struct Frame {
    code: u8,
    good: bool,
    /// Raw soft decision and the two amplitudes at each sampling instant.
    samples: [(f32, f32, f32); 7],
}

/// Recent demodulator output, (soft, hi, lo) by output sample index, so a
/// character can be sampled again once its own transitions have refined
/// where its bits lie.
struct Hist {
    v: Vec<(f32, f32, f32)>,
}

const HIST_LEN: usize = 1024;

impl Hist {
    fn new() -> Self {
        Self { v: vec![(0.0, 0.0, 0.0); HIST_LEN] }
    }

    fn put(&mut self, n: u64, x: (f32, f32, f32)) {
        self.v[n as usize % HIST_LEN] = x;
    }

    fn get(&self, n: u64) -> (f32, f32, f32) {
        self.v[n as usize % HIST_LEN]
    }

    /// Where the soft decision crosses zero near `around`: a straight line
    /// fitted to the samples within `w` of it. The matched filter's output
    /// ramps linearly through a transition, so the fit averages the noise
    /// of a dozen samples, where the first sign change is pulled early by
    /// whichever noise spike comes first. `falling` is the expected
    /// direction; a fit the other way, or one landing outside the window,
    /// is no estimate.
    fn crossing(&self, around: f64, w: f64, falling: bool) -> Option<f64> {
        let lo = (around - w).ceil().max(0.0) as u64;
        let hi = (around + w).floor() as u64;
        if hi <= lo + 2 {
            return None;
        }
        let (mut sx, mut sy, mut sxx, mut sxy, mut m) = (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for i in lo..=hi {
            let x = i as f64 - around;
            let y = self.get(i).0 as f64;
            sx += x;
            sy += y;
            sxx += x * x;
            sxy += x * y;
            m += 1.0;
        }
        let den = m * sxx - sx * sx;
        if den <= 0.0 {
            return None;
        }
        let b = (m * sxy - sx * sy) / den;
        let a = (sy - b * sx) / m;
        if b == 0.0 || (b < 0.0) != falling {
            return None;
        }
        let x = -a / b;
        (x.abs() <= w).then_some(around + x)
    }

    /// Linear interpolation at fractional sample time `t`, which must lie
    /// within the stored span.
    fn at(&self, t: f64) -> (f32, f32, f32) {
        let i = t.floor().max(0.0) as u64;
        let a = (t - i as f64) as f32;
        let (p, c) = (self.get(i), self.get(i + 1));
        (p.0 + (c.0 - p.0) * a, p.1 + (c.1 - p.1) * a, p.2 + (c.2 - p.2) * a)
    }
}

/// Bit timing shared by both polarities' framers, in output samples.
struct Timing {
    spb: f64,
    /// Half the matched filter: from a zero crossing of its output to the
    /// instant it holds the whole of the new bit.
    half_len: f64,
    /// Mark held before a crossing counts as a start edge.
    min_run: f32,
    /// Start-to-start spacing of characters sent back to back.
    period: f64,
    /// Half-width of the window a crossing is fitted over.
    fit: f64,
}

/// Start-bit framing for one polarity.
///
/// A start edge is taken from the matched filter's zero crossing. At low
/// SNR one crossing is a poor clock: noise brings it early as often as not,
/// and sampling a sixth of a bit early costs about as much as 2 dB of SNR.
/// Two things firm it up. Characters sent back to back start one period
/// apart, so a crossing near where the last character predicts the next is
/// pulled most of the way to the prediction. And once a character has
/// arrived, the crossings at its own bit transitions say where its bits
/// really lie, and it is sampled again there.
///
/// Machine-sent RTTY (weather and press broadcasts, contest macros) is
/// synchronous: characters follow each other on a fixed clock. After a few
/// clean characters in a row on that clock, the framer stops hunting and
/// runs on it, the way a flywheel keeps turning: each character is taken
/// where the clock says, nudged by its start edge if one is seen nearby,
/// and decoded whether or not its start and stop bits survived. A fade
/// that wipes out a start bit then costs that character rather than the
/// several that follow while a hunting framer slips through data bits.
/// Four failed characters in a row, or three character times of idle mark,
/// and it hunts again.
struct Framer {
    /// +1 when the upper tone is mark.
    sign: f32,
    hunting: bool,
    /// A start crossing seen, waiting for the samples to fit it.
    seen: Option<f64>,
    checked: bool,
    mark_run: f32,
    /// The start edge's crossing time.
    t0: f64,
    /// The last character's, when it framed cleanly: where to look for the
    /// next.
    expect: Option<f64>,
    /// Start-to-start spacing of characters sent back to back, as heard.
    period: f64,
    /// The last clean character's start, which characters sent back to
    /// back keep to a grid of `period`.
    grid: Option<f64>,
    /// This character's start came where the last one predicted.
    chained: bool,
    /// Clean characters in a row, each where the last predicted.
    run: u32,
    /// Running on the character clock: see [`Framer`].
    locked: bool,
    /// While locked: characters in a row that failed to frame, and
    /// character times in a row of idle mark.
    misses: u32,
    idle: u32,
    shift: Shift,
    q: f32,
    frames: u32,
    pw_on: f32,
    pw_off: f32,
    preroll: VecDeque<char>,
}

impl Framer {
    fn new(sign: f32, period: f64) -> Self {
        Self {
            sign,
            hunting: true,
            seen: None,
            checked: false,
            mark_run: 0.0,
            t0: 0.0,
            expect: None,
            period,
            grid: None,
            chained: false,
            run: 0,
            locked: false,
            misses: 0,
            idle: 0,
            shift: Shift::default(),
            q: 0.5,
            frames: 0,
            pw_on: 0.0,
            pw_off: 0.0,
            preroll: VecDeque::with_capacity(PREROLL),
        }
    }

    fn ratio(&self) -> f32 {
        self.pw_on / (self.pw_off + TINY)
    }

    fn unlock(&mut self) {
        self.locked = false;
        self.run = 0;
        self.expect = None;
    }

    /// Advance to output sample `n`, already stored in `h`.
    fn step(&mut self, n: u64, h: &Hist, tm: &Timing) -> Option<Frame> {
        if n == 0 {
            return None;
        }
        let v = self.sign * h.get(n).0;
        let vp = self.sign * h.get(n - 1).0;
        let falling = self.sign > 0.0;
        if self.hunting {
            if let Some(t) = self.expect {
                // Text sent back to back puts the next start edge one,
                // one and a half or two stop bits after this character's
                // stop bit began. Each of those places is tried, and the
                // one with the clearest mark-to-space step taken, rather
                // than the first noise dip after the stop bit.
                let span = 8.0 * tm.spb + tm.half_len + tm.fit + 2.0;
                if (n as f64) < t + span {
                    return None;
                }
                self.expect = None;
                if self.locked {
                    let p = t + self.period;
                    // Mark where the start bit should be, with no step
                    // down into it, is a gap in the sending, not a start bit
                    // lost in a fade: the station paused (DWD does, for a
                    // bit or two, between its RY run and the text). The
                    // next character starts wherever the line next drops
                    // to space, which may already be in the history.
                    let step = self.sign * (h.at(p - tm.half_len).0 - h.at(p + tm.half_len).0);
                    if self.sign * h.at(p + tm.half_len).0 > 0.6 && step < 0.3 {
                        self.idle += 1;
                        if self.idle >= 3 {
                            self.unlock();
                        }
                        self.grid = None;
                        let from = (p - 0.3 * tm.spb).max(1.0).ceil() as u64;
                        let edge = (from..=n).find_map(|i| {
                            let (a, b) = (self.sign * h.get(i - 1).0, self.sign * h.get(i).0);
                            (a > 0.0 && b <= 0.0).then(|| (i - 1) as f64 + (a / (a - b)) as f64)
                        });
                        match edge {
                            Some(tc) => {
                                self.seen = Some(tc);
                                self.chained = false;
                                self.hunting = false;
                                self.checked = false;
                            }
                            None => self.mark_run = (2.0 * tm.spb) as f32,
                        }
                        return None;
                    }
                    let fit = h.crossing(p, tm.fit, falling).filter(|f| (f - p).abs() < 0.25 * tm.spb);
                    if let Some(f) = fit {
                        self.period += 0.05 * (f - t - self.period);
                    }
                    self.t0 = fit.map_or(p, |f| p + 0.3 * (f - p));
                    self.chained = true;
                    self.hunting = false;
                    self.checked = true;
                    return None;
                }
                let step_at = |c: f64| self.sign * (h.at(c - tm.half_len).0 - h.at(c + tm.half_len).0);
                let mut best: Option<(f32, f64, bool)> = None;
                for stop in [1.0, 1.5, 2.0] {
                    let p = t + (6.0 + stop) * tm.spb;
                    let fit = h.crossing(p, tm.fit, falling).filter(|f| (f - p).abs() < 0.3 * tm.spb);
                    let c = fit.map_or(p, |f| p + 0.5 * (f - p));
                    // The stop length this station has been sending is
                    // favored over the others.
                    let usual = (p - (t + self.period)).abs() < 0.25 * tm.spb;
                    let score = step_at(c) + if usual { 0.3 } else { 0.0 };
                    if best.is_none_or(|(b, _, _)| score > b) {
                        best = Some((score, c, usual && fit.is_some()));
                    }
                }
                match best {
                    // Any step will do: if it was noise in the idle mark,
                    // the start bit's own check turns it away.
                    Some((score, c, usual)) if score > 0.0 => {
                        self.period += 0.2 * ((c - t) - self.period);
                        self.grid = Some(c);
                        self.t0 = c;
                        self.chained = usual;
                        self.hunting = false;
                        self.checked = false;
                    }
                    _ => {
                        // Idle, or a gap: hunt freely from here.
                        self.mark_run = if v > 0.0 { tm.min_run } else { 0.0 };
                    }
                }
                return None;
            }
            if vp > 0.0 && v <= 0.0 && self.mark_run >= tm.min_run {
                let tc = (n - 1) as f64 + (vp / (vp - v)) as f64;
                // Soon after a clean character, a crossing off its
                // character grid with only a short run of mark before it is
                // far more likely a data transition, after a character was
                // lost, than a start.
                let off_grid = self.grid.is_some_and(|g| {
                    let k = ((tc - g) / self.period).round();
                    k <= 4.0 && (tc - g - k * self.period).abs() > 0.3 * tm.spb
                });
                if off_grid && (self.mark_run as f64) < 1.4 * tm.spb {
                    self.mark_run = 0.0;
                    return None;
                }
                self.seen = Some(tc);
                self.chained = false;
                self.hunting = false;
                self.checked = false;
            } else {
                self.mark_run = if v > 0.0 { self.mark_run + 1.0 } else { 0.0 };
            }
            return None;
        }
        if let Some(seen) = self.seen {
            if (n as f64) < seen + tm.fit + 1.0 {
                return None;
            }
            self.seen = None;
            // Fit the edge twice, the second time centered on the first fit.
            let mut edge = seen;
            for _ in 0..2 {
                if let Some(t) = h.crossing(edge, tm.fit, falling) {
                    edge = t;
                }
            }
            self.t0 = edge;
        }
        let first = self.t0 + tm.half_len;
        if !self.checked && n as f64 >= first + 1.0 {
            if self.sign * h.at(first).0 >= 0.0 {
                // Not a start bit after all.
                self.hunting = true;
                self.mark_run = 0.0;
                return None;
            }
            self.checked = true;
        }
        // Wait until the stop bit can be sampled even if the transitions
        // move the character late.
        if (n as f64) < first + 6.4 * tm.spb + 1.0 {
            return None;
        }
        let bit = |t0: f64, k: usize| self.sign * h.at(t0 + tm.half_len + k as f64 * tm.spb).0 > 0.0;
        let mut prev = bit(self.t0, 0);
        let (mut sum, mut cnt) = (0.0f64, 0u32);
        for k in 1..7 {
            let cur = bit(self.t0, k);
            if cur != prev {
                // The raw decision falls when the line goes from the upper
                // tone to the lower.
                let want = self.t0 + k as f64 * tm.spb;
                let falling = (self.sign > 0.0) == prev;
                if let Some(tc) = h.crossing(want, tm.fit, falling).filter(|tc| (tc - want).abs() < 0.4 * tm.spb) {
                    sum += tc - want;
                    cnt += 1;
                }
            }
            prev = cur;
        }
        // The start crossing is one more observation, with no residual.
        let t0 = self.t0 + (sum / (cnt + 1) as f64).clamp(-0.35 * tm.spb, 0.35 * tm.spb);
        let mut samples = [(0.0, 0.0, 0.0); 7];
        let mut code = 0u8;
        for (k, s) in samples.iter_mut().enumerate() {
            *s = h.at(t0 + tm.half_len + k as f64 * tm.spb);
            if (1..=5).contains(&k) && self.sign * s.0 > 0.0 {
                code |= 1 << (k - 1);
            }
        }
        let good = self.sign * samples[6].0 > 0.0 && self.sign * samples[0].0 < 0.0;
        self.hunting = true;
        self.mark_run = if good { tm.min_run } else { 0.0 };
        if good {
            self.grid = Some(t0);
        }
        if self.locked {
            if samples.iter().all(|s| self.sign * s.0 > 0.0) {
                // Mark where a character was due. Firm mark is the station
                // pausing, and whatever comes next starts off the clock, so
                // it is hunted for; weak mark is a fade, and the clock is
                // the best guess at where the next character lies.
                self.idle += 1;
                if self.idle >= 3 {
                    self.unlock();
                } else if samples.iter().all(|s| self.sign * s.0 > 0.6) {
                    self.grid = None;
                    self.mark_run = (2.0 * tm.spb) as f32;
                } else {
                    self.expect = Some(t0);
                }
                return None;
            }
            self.idle = 0;
            self.misses = if good { 0 } else { self.misses + 1 };
            if self.misses >= 4 {
                self.unlock();
            } else {
                self.expect = Some(t0);
            }
        } else {
            self.run = match (good, self.chained) {
                (true, true) => self.run + 1,
                (true, false) => 1,
                (false, _) => 0,
            };
            self.locked = self.run >= 3;
            self.misses = 0;
            self.idle = 0;
            self.expect = good.then_some(t0);
        }
        Some(Frame { code, good, samples })
    }
}

pub struct RttyDecoder {
    fs: f32,
    cfg: RttyConfig,
    rx: FskReceiver,
    samples: Vec<FskSample>,
    atc: Atc,
    framers: [Framer; 2],
    active: usize,
    n: u64,
    hist: Hist,
    timing: Timing,
    open: bool,
    /// When the squelch was last seen open.
    last_open: Option<u64>,
    us_figs: bool,
    lines: Lines,
    /// Characters out of the last frame.
    chars_buf: Vec<char>,
    events: Vec<RttyText>,
    t_in: u64,
    last_char: u64,
    chars: u64,
}

impl RttyDecoder {
    pub fn new(fs: f32, cfg: RttyConfig) -> Self {
        let rx = FskReceiver::new(fs, cfg.baud, cfg.shift_hz, cfg.center_hz);
        let spb = rx.samples_per_bit() as f64;
        let half_len = 0.5 * rx.filter_len() as f64;
        let period = (6.0 + cfg.stop_bits.max(1.0) as f64) * spb;
        Self {
            fs,
            // US teleprinter figures belong to 45.45 baud amateur work;
            // 50 baud and up is commercial and weather traffic in the
            // international alphabet.
            us_figs: (cfg.baud - 45.45).abs() < 0.5,
            active: usize::from(cfg.reverse == Some(true)),
            cfg,
            rx,
            samples: Vec::new(),
            atc: Atc::new(spb as f32),
            framers: [Framer::new(1.0, period), Framer::new(-1.0, period)],
            n: 0,
            hist: Hist::new(),
            timing: Timing {
                spb,
                half_len,
                min_run: (0.3 * spb) as f32,
                period,
                fit: 0.4 * 2.0 * half_len,
            },
            open: false,
            last_open: None,
            lines: Lines::default(),
            chars_buf: Vec::with_capacity(GUARD_MAX + 1),
            events: Vec::new(),
            t_in: 0,
            last_char: 0,
            chars: 0,
        }
    }

    /// Change the mode. Pending text is let go first; the search starts over.
    pub fn set_config(&mut self, cfg: RttyConfig) {
        self.flush();
        let events = std::mem::take(&mut self.events);
        let chars = self.chars;
        *self = Self::new(self.fs, cfg);
        self.events = events;
        self.chars = chars;
    }

    pub fn push(&mut self, audio: &[f32]) {
        let mut samples = std::mem::take(&mut self.samples);
        self.rx.push(audio, &mut samples);
        for s in &samples {
            if s.fresh {
                self.restart();
            }
            self.hist.put(self.n, (self.atc.step(s.hi, s.lo), s.hi, s.lo));
            for p in 0..2 {
                if self.cfg.reverse.is_some() && p != self.active {
                    continue;
                }
                if let Some(f) = self.framers[p].step(self.n, &self.hist, &self.timing) {
                    self.on_frame(p, f);
                }
            }
            self.n += 1;
        }
        self.samples = samples;
        self.t_in += audio.len() as u64;
        if self.open {
            self.last_open = Some(self.t_in);
        }
        if !self.rx.present() {
            // Whatever was held through a fade belonged to a signal that
            // has now gone.
            self.open = false;
            if self.last_open.is_some() {
                self.last_open = None;
                self.framers.iter_mut().for_each(|f| f.preroll.clear());
            }
        }
        self.rx.set_tracking(self.open);
        if (self.t_in - self.last_char) as f32 > IDLE_FLUSH_S * self.fs {
            // A word held after a figure shift is let go when the station
            // falls quiet in the middle of it.
            let mut chars = std::mem::take(&mut self.chars_buf);
            chars.clear();
            let a = self.active;
            self.framers[a].shift.flush(self.cfg.unshift_on_space, self.us_figs, &mut chars);
            if self.open {
                for &c in &chars {
                    self.print(c);
                }
            }
            self.chars_buf = chars;
            if !self.lines.is_empty() {
                self.flush();
            }
        }
    }

    pub fn take(&mut self) -> Vec<RttyText> {
        std::mem::take(&mut self.events)
    }

    pub fn status(&self) -> RttyStatus {
        RttyStatus {
            locked: self.rx.locked(),
            printing: self.open,
            center_hz: self.rx.center_hz(),
            reverse: self.framers[self.active].sign < 0.0,
            snr_db: self.rx.snr_db(),
            chars: self.chars,
            frame_quality: self.framers[self.active].q,
        }
    }

    /// A new lock: nothing framed so far belongs to it.
    fn restart(&mut self) {
        self.atc.reset();
        self.framers = [Framer::new(1.0, self.timing.period), Framer::new(-1.0, self.timing.period)];
        self.active = usize::from(self.cfg.reverse == Some(true));
        self.open = false;
        self.n = 0;
        self.hist = Hist::new();
    }

    fn on_frame(&mut self, p: usize, f: Frame) {
        let fr = &mut self.framers[p];
        fr.frames += 1;
        fr.q += (if f.good { 1.0 } else { 0.0 } - fr.q) / Q_LEN;
        let a = if fr.frames == 1 { 1.0 } else { 1.0 / 21.0 };
        for &(_, h, l) in &f.samples {
            let (on, off) = if h > l { (h, l) } else { (l, h) };
            fr.pw_on += a * (on * on - fr.pw_on);
            fr.pw_off += a * (off * off - fr.pw_off);
        }
        // Only a run of clean, strong frames is worth printing from the
        // held-back text: whatever framed before the signal began was noise.
        let strong = f.samples.iter().all(|&(_, h, l)| h.max(l) >= 2.5 * h.min(l));
        let fading = self.last_open.is_some_and(|t| (self.t_in - t) as f32 <= FADE_HOLD_S * self.fs) && self.rx.present();
        if !(fading || f.good && strong) {
            fr.preroll.clear();
        }
        let mut chars = std::mem::take(&mut self.chars_buf);
        chars.clear();
        fr.shift.feed(ITA2_INDEX[f.code as usize] as usize, self.cfg.unshift_on_space, self.us_figs, &mut chars);
        for &c in &chars {
            if p == self.active && self.open {
                self.print(c);
            } else if (f.good && strong) || (fading && p == self.active) {
                let held = &mut self.framers[p].preroll;
                if held.len() == PREROLL {
                    held.pop_front();
                }
                held.push_back(c);
            }
        }
        self.chars_buf = chars;
        // A wrong polarity frames on transitions inside characters and
        // fails far more stop bits than the right one.
        if self.cfg.reverse.is_none() {
            let o = 1 - self.active;
            if self.framers[o].frames >= 10 && self.framers[o].q > self.framers[self.active].q + 0.2 {
                self.active = o;
                self.open = false;
            }
        }
        let fa = &self.framers[self.active];
        if !self.open {
            // Until the polarity is forced or settled, the other one must
            // be framing worse.
            let fo = &self.framers[1 - self.active];
            let better = self.cfg.reverse.is_some() || fa.q > fo.q;
            if better && self.rx.present() && fa.frames >= 6 && fa.q >= OPEN_Q && fa.ratio() >= OPEN_RATIO {
                self.open = true;
                let held: Vec<char> = self.framers[self.active].preroll.drain(..).collect();
                for c in held {
                    self.print(c);
                }
            }
        } else if !self.rx.present() || fa.q < CLOSE_Q || fa.ratio() < CLOSE_RATIO {
            self.open = false;
        }
    }

    fn print(&mut self, c: char) {
        self.last_char = self.t_in;
        if c != '\r' && c != '\n' {
            self.chars += 1;
        }
        if let Some(text) = self.lines.push(c) {
            self.emit(text);
        }
    }

    fn flush(&mut self) {
        if let Some(text) = self.lines.take() {
            self.emit(text);
        }
    }

    fn emit(&mut self, text: String) {
        self.events.push(RttyText {
            text,
            center_hz: self.rx.center_hz(),
            snr_db: self.rx.snr_db(),
            reverse: self.framers[self.active].sign < 0.0,
        });
    }
}

// ---------------------------------------------------------------------------
// Test signals, shared with the NAVTEX tests.

#[cfg(test)]
pub(crate) mod testsig {
    /// xorshift64*, so the tests need no extra crate and repeat exactly.
    pub struct Rng(u64);

    impl Rng {
        pub fn new(seed: u64) -> Self {
            Self(seed.max(1))
        }

        pub fn uniform(&mut self) -> f64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            ((self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64 + 0.5) / (1u64 << 53) as f64
        }

        pub fn gauss(&mut self) -> f32 {
            let (u, v) = (self.uniform(), self.uniform());
            ((-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()) as f32
        }
    }

    /// Continuous-phase FSK: one entry per symbol, true for the upper tone.
    pub fn fsk(levels: &[bool], rate: f32, fs: f32, center: f32, shift: f32, amp: f32) -> Vec<f32> {
        let n = (levels.len() as f64 * fs as f64 / rate as f64) as usize;
        let mut out = Vec::with_capacity(n);
        let mut ph = 0.0f64;
        for i in 0..n {
            let k = ((i as f64 * rate as f64 / fs as f64) as usize).min(levels.len() - 1);
            let f = center as f64 + if levels[k] { 0.5 } else { -0.5 } * shift as f64;
            out.push((amp as f64 * ph.cos()) as f32);
            ph = (ph + std::f64::consts::TAU * f / fs as f64) % std::f64::consts::TAU;
        }
        out
    }

    /// The analytic (complex) form of [`fsk`] at unit amplitude.
    pub fn fsk_iq(levels: &[bool], rate: f32, fs: f32, center: f32, shift: f32) -> Vec<num_complex::Complex32> {
        let n = (levels.len() as f64 * fs as f64 / rate as f64) as usize;
        let mut out = Vec::with_capacity(n);
        let mut ph = 0.0f64;
        for i in 0..n {
            let k = ((i as f64 * rate as f64 / fs as f64) as usize).min(levels.len() - 1);
            let f = center as f64 + if levels[k] { 0.5 } else { -0.5 } * shift as f64;
            out.push(num_complex::Complex32::new(ph.cos() as f32, ph.sin() as f32));
            ph = (ph + std::f64::consts::TAU * f / fs as f64) % std::f64::consts::TAU;
        }
        out
    }

    /// A Rayleigh-fading path gain: complex Gaussian with a Gaussian
    /// Doppler spectrum whose frequency spread (twice its standard
    /// deviation, as Watterson and CCIR Rec. 520 define it) is `spread_hz`,
    /// unit mean power, `n` samples at `fs`.
    pub fn rayleigh(n: usize, fs: f32, spread_hz: f32, rng: &mut Rng) -> Vec<num_complex::Complex32> {
        use num_complex::Complex32;
        let fr = 64.0f64;
        let sd_f = 0.5 * spread_hz as f64;
        let sd_t = 1.0 / (2.0 * std::f64::consts::SQRT_2 * std::f64::consts::PI * sd_f);
        let half = (3.0 * sd_t * fr).ceil() as i64;
        let taps: Vec<f64> = (-half..=half).map(|k| (-(k as f64 / fr).powi(2) / (2.0 * sd_t * sd_t)).exp()).collect();
        let m = (n as f64 / fs as f64 * fr) as usize + 3;
        let white: Vec<Complex32> = (0..m + taps.len()).map(|_| Complex32::new(rng.gauss(), rng.gauss())).collect();
        let mut slow: Vec<Complex32> = (0..m)
            .map(|i| taps.iter().enumerate().map(|(k, t)| white[i + k] * *t as f32).sum())
            .collect();
        let p = slow.iter().map(|g| g.norm_sqr() as f64).sum::<f64>() / m as f64;
        let norm = (1.0 / p).sqrt() as f32;
        slow.iter_mut().for_each(|g| *g *= norm);
        (0..n)
            .map(|i| {
                let t = i as f64 / fs as f64 * fr;
                let k = t as usize;
                let a = (t - k as f64) as f32;
                slow[k] * (1.0 - a) + slow[k + 1] * a
            })
            .collect()
    }

    /// HF propagation for the tests.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub enum Channel {
        Awgn,
        /// Two equal Rayleigh paths (Watterson), delay in ms, spread in Hz:
        /// CCIR moderate is (1.0, 0.5), poor (2.0, 1.0).
        Watterson(f32, f32),
        /// Each tone fading on its own (spread in Hz): the extreme of
        /// selective fading, where one tone can vanish while the other holds.
        Selective(f32),
    }

    impl Channel {
        pub const MODERATE: Channel = Channel::Watterson(1.0, 0.5);
        pub const POOR: Channel = Channel::Watterson(2.0, 1.0);

        pub fn name(&self) -> String {
            match self {
                Channel::Awgn => "AWGN".into(),
                Channel::Watterson(d, s) if (*d, *s) == (1.0, 0.5) => "CCIR moderate".into(),
                Channel::Watterson(d, s) if (*d, *s) == (2.0, 1.0) => "CCIR poor".into(),
                Channel::Watterson(d, s) => format!("{d} ms / {s} Hz"),
                Channel::Selective(s) => format!("selective {s} Hz"),
            }
        }
    }

    /// FSK at `amp` (mean power that of a sine of `amp`) through `ch`.
    #[allow(clippy::too_many_arguments)]
    pub fn fsk_through(levels: &[bool], rate: f32, fs: f32, center: f32, shift: f32, amp: f32, ch: Channel, rng: &mut Rng) -> Vec<f32> {
        let s = fsk_iq(levels, rate, fs, center, shift);
        match ch {
            Channel::Awgn => s.iter().map(|z| amp * z.re).collect(),
            Channel::Watterson(delay_ms, spread) => {
                let g1 = rayleigh(s.len(), fs, spread, rng);
                let g2 = rayleigh(s.len(), fs, spread, rng);
                let d = (delay_ms * 1e-3 * fs) as usize;
                let k = amp * std::f32::consts::FRAC_1_SQRT_2;
                (0..s.len())
                    .map(|i| {
                        let late = if i >= d { s[i - d] * g2[i] } else { num_complex::Complex32::new(0.0, 0.0) };
                        k * (s[i] * g1[i] + late).re
                    })
                    .collect()
            }
            Channel::Selective(spread) => {
                let gh = rayleigh(s.len(), fs, spread, rng);
                let gl = rayleigh(s.len(), fs, spread, rng);
                (0..s.len())
                    .map(|i| {
                        let k = ((i as f64 * rate as f64 / fs as f64) as usize).min(levels.len() - 1);
                        amp * (s[i] * if levels[k] { gh[i] } else { gl[i] }).re
                    })
                    .collect()
            }
        }
    }

    /// Add white noise for `snr_db` in 2500 Hz against a sine of `amp`.
    pub fn add_noise(x: &mut [f32], amp: f32, snr_db: f32, fs: f32, rng: &mut Rng) {
        let sigma = noise_sigma(amp, snr_db, fs);
        for v in x.iter_mut() {
            *v += sigma * rng.gauss();
        }
    }

    pub fn noise_sigma(amp: f32, snr_db: f32, fs: f32) -> f32 {
        let p_sig = 0.5 * amp * amp;
        let p_2500 = p_sig / 10f32.powf(snr_db / 10.0);
        (p_2500 * (0.5 * fs) / 2500.0).sqrt()
    }

    /// Feed in irregular chunks, as the server does.
    pub fn chunks(x: &[f32], mut f: impl FnMut(&[f32])) {
        const SIZES: [usize; 6] = [480, 960, 333, 2000, 717, 1440];
        let mut i = 0;
        let mut k = 0;
        while i < x.len() {
            let n = SIZES[k % SIZES.len()].min(x.len() - i);
            f(&x[i..i + n]);
            i += n;
            k += 1;
        }
    }

    /// Edit distance, for character error rates.
    pub fn levenshtein(a: &str, b: &str) -> usize {
        let a: Vec<char> = a.chars().collect();
        let b: Vec<char> = b.chars().collect();
        let mut row: Vec<usize> = (0..=b.len()).collect();
        for (i, ca) in a.iter().enumerate() {
            let mut prev = row[0];
            row[0] = i + 1;
            for (j, cb) in b.iter().enumerate() {
                let cur = row[j + 1];
                row[j + 1] = (prev + usize::from(ca != cb)).min(row[j] + 1).min(row[j + 1] + 1);
                prev = cur;
            }
        }
        row[b.len()]
    }
}

#[cfg(test)]
mod tests {
    use super::testsig::*;
    use super::*;

    const FS: f32 = 48_000.0;

    /// ITA2 codes for `text`, with shifts, assuming the receiver unshifts
    /// on space.
    /// ITA2 codes for `text`, with the shifts a receiver needs that does
    /// (`usos`) or does not unshift on space.
    fn encode_usos(text: &str, usos: bool) -> Vec<u8> {
        let letter = |c: char| SYMS.iter().find(|s| s.letter == c);
        let figure = |c: char| SYMS.iter().find(|s| s.figure == c);
        let ltrs = letter(LTRS).unwrap().ita2;
        let figs_code = letter(FIGS).unwrap().ita2;
        let mut out = vec![ltrs, ltrs];
        let mut figs = false;
        for c in text.chars() {
            if matches!(c, ' ' | '\r' | '\n') {
                out.push(letter(c).unwrap().ita2);
                figs &= !(usos && c == ' ');
            } else if let Some(s) = letter(c) {
                if figs {
                    out.push(ltrs);
                    figs = false;
                }
                out.push(s.ita2);
            } else if let Some(s) = figure(c) {
                if !figs {
                    out.push(figs_code);
                    figs = true;
                }
                out.push(s.ita2);
            } else {
                panic!("no ITA2 code for {c:?}");
            }
        }
        out
    }

    /// Half-bit levels (true = mark) for the frames, idle mark around them.
    fn frames(codes: &[u8], stop_bits: f32, lead_s: f32, tail_s: f32, baud: f32) -> Vec<bool> {
        let halves = |s: f32| (s * baud * 2.0) as usize;
        let mut v = vec![true; halves(lead_s)];
        let stop = (stop_bits * 2.0).round() as usize;
        for &c in codes {
            v.extend([false, false]);
            for b in 0..5 {
                let m = c >> b & 1 == 1;
                v.extend([m, m]);
            }
            v.extend(std::iter::repeat_n(true, stop));
        }
        v.extend(std::iter::repeat_n(true, halves(tail_s)));
        v
    }

    struct Case {
        cfg: RttyConfig,
        center: f32,
        reverse: bool,
        amp: f32,
        snr_db: Option<f32>,
        seed: u64,
    }

    fn signal_through(text: &str, c: &Case, ch: Channel) -> Vec<f32> {
        let mut rng = Rng::new(c.seed);
        let levels: Vec<bool> = frames(&encode_usos(text, c.cfg.unshift_on_space), c.cfg.stop_bits, 1.0, 3.0, c.cfg.baud)
            .into_iter()
            .map(|m| m != c.reverse)
            .collect();
        let mut x = vec![0.0; FS as usize];
        x.extend(fsk_through(&levels, 2.0 * c.cfg.baud, FS, c.center, c.cfg.shift_hz, c.amp, ch, &mut rng));
        add_noise(&mut x, c.amp, c.snr_db.unwrap_or(30.0), FS, &mut rng);
        x
    }

    /// Character error rate over three seeds.
    pub(super) fn cer(cfg: &RttyConfig, ch: Channel, snr: f32) -> f32 {
        let text = TEXT.repeat(3);
        let expect = [EXPECT; 3].join("\n");
        let (mut errs, mut total) = (0, 0);
        let seeds: u64 = std::env::var("USDR_CER_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(3);
        for seed in 1..=seeds {
            let c = Case { cfg: cfg.clone(), center: 1500.0, reverse: false, amp: 0.3, snr_db: Some(snr), seed: 100 + seed };
            let (got, _) = run(&signal_through(&text, &c, ch), cfg.clone());
            errs += levenshtein(&joined(&got), &expect);
            total += expect.len();
        }
        errs as f32 / total as f32
    }

    #[test]
    fn rtty_cer_table() {
        if std::env::var("USDR_CER_TABLE").is_err() {
            return;
        }
        // DWD itself does not unshift on space, but the table compares like
        // with like: both modes are sent and received with it.
        for cfg in [RttyConfig::amateur(), RttyConfig { unshift_on_space: true, ..RttyConfig::dwd() }] {
            println!("RTTY {}/{}: CER % by SNR (2500 Hz)", cfg.baud, cfg.shift_hz);
            let snrs = [10.0f32, 5.0, 0.0, -3.0, -5.0, -8.0];
            let chans: Vec<Channel> = if std::env::var("USDR_CER_FADING").is_ok() { vec![Channel::MODERATE, Channel::POOR, Channel::Selective(0.5)] } else { vec![Channel::Awgn, Channel::MODERATE, Channel::POOR, Channel::Selective(0.5)] };
            println!("{:>18} {}", "", snrs.iter().map(|s| format!("{s:>7.0}")).collect::<String>());
            for &ch in &chans {
                let row: String = snrs.iter().map(|&s| format!("{:>7.1}", 100.0 * cer(&cfg, ch, s))).collect();
                println!("{:>18} {row}", ch.name());
            }
        }
    }

    fn signal(text: &str, c: &Case) -> Vec<f32> {
        let mut rng = Rng::new(c.seed);
        let levels: Vec<bool> = frames(&encode_usos(text, c.cfg.unshift_on_space), c.cfg.stop_bits, 1.0, 3.0, c.cfg.baud)
            .into_iter()
            .map(|m| m != c.reverse)
            .collect();
        // A second of noise first, so the pair has to be found from nothing.
        let lead = (FS * 1.0) as usize;
        let mut x = vec![0.0; lead];
        x.extend(fsk(&levels, 2.0 * c.cfg.baud, FS, c.center, c.cfg.shift_hz, c.amp));
        match c.snr_db {
            Some(snr) => add_noise(&mut x, c.amp, snr, FS, &mut rng),
            None => add_noise(&mut x, c.amp, 30.0, FS, &mut rng),
        }
        x
    }

    fn run(x: &[f32], cfg: RttyConfig) -> (Vec<RttyText>, RttyStatus) {
        let mut d = RttyDecoder::new(FS, cfg);
        let mut got = Vec::new();
        chunks(x, |c| {
            d.push(c);
            got.extend(d.take());
        });
        (got, d.status())
    }

    fn joined(t: &[RttyText]) -> String {
        t.iter().map(|t| t.text.as_str()).collect::<Vec<_>>().join("\n")
    }

    const TEXT: &str = "RYRYRYRY CQ CQ DE W1AW W1AW K\rTHE QUICK BROWN FOX JUMPS OVER THE LAZY DOG 0123456789\r\nNOW IS THE TIME, 1-2/3 (OK) 73.\r\n";
    const EXPECT: &str = "RYRYRYRY CQ CQ DE W1AW W1AW K\nTHE QUICK BROWN FOX JUMPS OVER THE LAZY DOG 0123456789\nNOW IS THE TIME, 1-2/3 (OK) 73.";

    fn case(cfg: RttyConfig) -> Case {
        Case { cfg, center: 2210.0, reverse: false, amp: 0.3, snr_db: None, seed: 7 }
    }

    #[test]
    fn rtty_round_trips_clean() {
        let c = case(RttyConfig::amateur());
        let (got, st) = run(&signal(TEXT, &c), c.cfg.clone());
        assert_eq!(joined(&got), EXPECT);
        assert!(!st.reverse);
        assert!((st.center_hz - 2210.0).abs() < 3.0, "center {}", st.center_hz);
        assert!(got.iter().all(|t| !t.reverse));
    }

    #[test]
    fn rtty_text_arrives_in_lines() {
        let c = case(RttyConfig::amateur());
        let (got, _) = run(&signal(TEXT, &c), c.cfg.clone());
        let lines: Vec<&str> = got.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(lines, EXPECT.lines().collect::<Vec<_>>());
        // A long line is cut at 80 characters, and text with no newline is
        // let go once the station falls quiet.
        let long = "ABCDEFGHIJ".repeat(9) + " END";
        let (got, _) = run(&signal(&long, &c), c.cfg.clone());
        let lines: Vec<&str> = got.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(lines, vec![&long[..80], &long[80..]]);
    }

    #[test]
    fn rtty_finds_an_offset_pair() {
        let mut c = case(RttyConfig::amateur());
        c.center = 2210.0 - 300.0;
        c.amp = 2e-4;
        c.snr_db = Some(10.0);
        let (got, st) = run(&signal(TEXT, &c), c.cfg.clone());
        assert_eq!(joined(&got), EXPECT);
        assert!((st.center_hz - 1910.0).abs() < 3.0, "center {}", st.center_hz);
    }

    #[test]
    fn rtty_detects_reversed_polarity() {
        let mut c = case(RttyConfig::amateur());
        c.reverse = true;
        c.snr_db = Some(10.0);
        let (got, st) = run(&signal(TEXT, &c), c.cfg.clone());
        assert_eq!(joined(&got), EXPECT);
        assert!(st.reverse);
        assert!(got.iter().all(|t| t.reverse));
        // Forcing the polarity decodes it too.
        let cfg = RttyConfig { reverse: Some(true), ..RttyConfig::amateur() };
        let (got, _) = run(&signal(TEXT, &c), cfg);
        assert_eq!(joined(&got), EXPECT);
    }

    #[test]
    fn rtty_fixed_center() {
        let mut c = case(RttyConfig::amateur());
        c.center = 1000.0;
        c.snr_db = Some(10.0);
        let cfg = RttyConfig { center_hz: Some(1000.0), ..RttyConfig::amateur() };
        let (got, st) = run(&signal(TEXT, &c), cfg);
        assert_eq!(joined(&got), EXPECT);
        assert_eq!(st.center_hz, 1000.0);
    }

    #[test]
    fn rtty_dwd_and_wide() {
        for (cfg, center) in [(RttyConfig::dwd(), 1500.0), (RttyConfig::wide(), 1700.0)] {
            let mut c = case(cfg.clone());
            c.center = center;
            c.snr_db = Some(5.0);
            let (got, st) = run(&signal(TEXT, &c), cfg.clone());
            assert_eq!(joined(&got), EXPECT, "{cfg:?}");
            assert!((st.center_hz - center).abs() < 5.0, "center {}", st.center_hz);
        }
    }

    #[test]
    fn rtty_character_error_rate() {
        let text = TEXT.repeat(3);
        let expect = [EXPECT; 3].join("\n");
        for (snr, max_cer) in [(0.0, 0.005), (-5.0, 0.03), (-8.0, 0.25)] {
            let mut total = 0usize;
            let mut errs = 0usize;
            for seed in 1..=3 {
                let mut c = case(RttyConfig::amateur());
                c.center = 1500.0;
                c.snr_db = Some(snr);
                c.seed = seed;
                let (got, _) = run(&signal(&text, &c), c.cfg.clone());
                errs += levenshtein(&joined(&got), &expect);
                total += expect.len();
            }
            let cer = errs as f32 / total as f32;
            println!("RTTY 45.45/170 at {snr:+.0} dB SNR (2500 Hz): CER {:.2}% ({errs}/{total})", 100.0 * cer);
            assert!(cer <= max_cer, "CER {cer} at {snr} dB");
        }
    }

    #[test]
    fn rtty_holds_copy_through_fading() {
        // Measured over three seeds of 1050 characters each; the full
        // table is `rtty_cer_table`.
        for cfg in [RttyConfig::amateur(), RttyConfig { unshift_on_space: true, ..RttyConfig::dwd() }] {
            for (ch, snr, max) in [(Channel::MODERATE, 10.0, 0.04), (Channel::POOR, 10.0, 0.06), (Channel::Selective(0.5), 5.0, 0.03)] {
                let c = cer(&cfg, ch, snr);
                println!("RTTY {}/{} {} at {snr:+.0} dB: CER {:.1}%", cfg.baud, cfg.shift_hz, ch.name(), 100.0 * c);
                assert!(c <= max, "{} at {snr} dB: CER {c}", ch.name());
            }
        }
    }

    /// DWD's call-up: an RY run, a pause of about a bit and a half (the
    /// captures show it), then CR CR LF LTRS and the CQ, sent without
    /// unshift on space.
    fn callup_signal(seed: u64, snr: f32) -> Vec<f32> {
        let cfg = RttyConfig::dwd();
        let ltrs = SYMS.iter().find(|s| s.letter == LTRS).unwrap().ita2;
        let ry: Vec<u8> = "RY".repeat(30).chars().map(|c| SYMS.iter().find(|s| s.letter == c).unwrap().ita2).collect();
        let mut rest = encode_usos("\r\r\n", false);
        rest.drain(..2);
        rest.push(ltrs);
        rest.extend(encode_usos("CQ CQ CQ DE DDK2 DDH7 DDK9\r\nFREQUENCIES 4583 KHZ 10100.8 KHZ\r\n", false).into_iter().skip(2));
        let mut lv = frames(&ry, cfg.stop_bits, 1.0, 0.0, cfg.baud);
        lv.extend([true; 3]);
        lv.extend(frames(&rest, cfg.stop_bits, 0.0, 3.0, cfg.baud));
        let lv: Vec<bool> = lv.into_iter().map(|m| !m).collect();
        let mut rng = Rng::new(seed);
        let mut x = vec![0.0; FS as usize];
        x.extend(fsk(&lv, 2.0 * cfg.baud, FS, 1417.0, cfg.shift_hz, 0.2));
        add_noise(&mut x, 0.2, snr, FS, &mut rng);
        x
    }

    #[test]
    fn rtty_callup_after_the_ry_run() {
        for seed in 1..=4 {
            let (got, st) = run(&callup_signal(seed, 5.0), RttyConfig::dwd());
            let all = joined(&got);
            assert!(st.reverse);
            assert!(all.contains("RYRYRY\nCQ CQ CQ DE DDK2 DDH7 DDK9\nFREQUENCIES 4583 KHZ 10100.8 KHZ"), "{all:?}");
        }
    }

    #[test]
    fn a_misread_figure_shift_reverts() {
        let row = |c: char| SYMS.iter().position(|s| s.letter == c).unwrap();
        let fig = |c: char| SYMS.iter().position(|s| s.figure == c).unwrap();
        let read = |rows: &[usize]| {
            let mut sh = Shift::default();
            let mut out = Vec::new();
            for &r in rows {
                sh.feed(r, false, false, &mut out);
            }
            sh.flush(false, false, &mut out);
            out.into_iter().collect::<String>()
        };
        // A letter shift read as a figure shift ahead of CQ.
        let mut rows = vec![row(FIGS)];
        for c in "CQ CQ CQ DE".chars() {
            rows.push(row(c));
        }
        assert_eq!(read(&rows), "CQ CQ CQ DE");
        // Real figures keep printing as figures.
        let figs = |text: &str| {
            let mut rows = Vec::new();
            let mut in_figs = false;
            for c in text.chars() {
                if c.is_ascii_uppercase() {
                    if in_figs {
                        rows.push(row(LTRS));
                        in_figs = false;
                    }
                    rows.push(row(c));
                } else if c == ' ' {
                    rows.push(row(' '));
                } else {
                    if !in_figs {
                        rows.push(row(FIGS));
                        in_figs = true;
                    }
                    rows.push(fig(c));
                }
            }
            rows
        };
        for text in ["SEA: 0,5 METER.", "FREQUENCIES 4583 KHZ 7646 KHZ 10100.8 KHZ", "TIME, 1-2/3 (OK) 73.", "AT 12:30 UTC", "51-30N 001-20E", "W1AW"] {
            assert_eq!(read(&figs(text)), text);
        }
    }

    #[test]
    fn rtty_noise_alone_prints_nothing() {
        let mut total = 0usize;
        for (seed, amp) in [(11u64, 1e-4f32), (12, 0.05), (13, 0.8)] {
            let mut rng = Rng::new(seed);
            let x: Vec<f32> = (0..(60.0 * FS) as usize).map(|_| amp * rng.gauss()).collect();
            for cfg in [RttyConfig::amateur(), RttyConfig::dwd()] {
                let (got, st) = run(&x, cfg);
                let n: usize = got.iter().map(|t| t.text.len()).sum();
                assert!(!st.locked || n == 0);
                total += n;
            }
        }
        assert_eq!(total, 0, "noise printed {total} characters");
    }

    #[test]
    fn rtty_little_junk_after_the_signal_stops() {
        let c = Case { snr_db: Some(5.0), ..case(RttyConfig::amateur()) };
        let mut x = signal(TEXT, &c);
        let mut rng = Rng::new(99);
        let sigma = noise_sigma(c.amp, 5.0, FS);
        x.extend((0..(20.0 * FS) as usize).map(|_| sigma * rng.gauss()));
        let (got, _) = run(&x, c.cfg.clone());
        let all = joined(&got);
        assert!(all.starts_with(EXPECT), "{all:?}");
        let junk = all.len() - EXPECT.len();
        println!("characters after the signal: {junk}");
        assert!(junk <= 6, "{all:?}");
    }

    #[test]
    fn rtty_realtime_factor() {
        let c = Case { snr_db: Some(0.0), ..case(RttyConfig::amateur()) };
        let x = signal(&TEXT.repeat(4), &c);
        let mut d = RttyDecoder::new(FS, c.cfg.clone());
        let t = std::time::Instant::now();
        chunks(&x, |c| d.push(c));
        let el = t.elapsed().as_secs_f64();
        let audio = x.len() as f64 / FS as f64;
        println!("RTTY: {audio:.1} s of audio in {:.1} ms, {:.3}% of one core", el * 1e3, 100.0 * el / audio);
        assert!(el / audio < 0.05);
    }
}

#[cfg(test)]
mod live {
    use super::testsig::*;
    use super::*;

    pub(crate) fn read_iq_wav(path: &std::path::Path) -> Option<(f32, Vec<Complex32>)> {
        let b = std::fs::read(path).ok()?;
        if b.len() < 44 || &b[..4] != b"RIFF" || &b[8..12] != b"WAVE" {
            return None;
        }
        let mut at = 12;
        let mut fs = 0u32;
        while at + 8 <= b.len() {
            let len = u32::from_le_bytes(b[at + 4..at + 8].try_into().ok()?) as usize;
            let body = &b[at + 8..(at + 8 + len).min(b.len())];
            match &b[at..at + 4] {
                b"fmt " => fs = u32::from_le_bytes(body[4..8].try_into().ok()?),
                b"data" => {
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
                _ => {}
            }
            at += 8 + len + (len & 1);
        }
        None
    }

    /// Sideband audio as the server makes it.
    pub(crate) fn usb_audio(fs: f32, iq: &[Complex32]) -> Vec<f32> {
        let mut ssb = crate::ssb::SsbDemod::with_passband(fs as f64, crate::ssb::Sideband::Upper, 100.0, 3100.0);
        let (mut audio, mut linear) = (Vec::new(), Vec::new());
        let mut all = Vec::with_capacity(iq.len());
        for chunk in iq.chunks((fs * 0.02) as usize) {
            ssb.process_with_linear(chunk, &mut audio, &mut linear);
            all.extend_from_slice(&linear);
        }
        all
    }

    /// Signal power of the pair at `center` and the noise density per Hz,
    /// from a long averaged spectrum.
    pub(crate) fn measure(x: &[f32], fs: f32, center: f32, shift: f32, baud: f32) -> (f64, f64) {
        let n = 8192;
        let fft = rustfft::FftPlanner::new().plan_fft_forward(n);
        let win: Vec<f32> = (0..n).map(|i| 0.5 - 0.5 * (TAU * i as f32 / n as f32).cos()).collect();
        let wpow: f64 = win.iter().map(|w| (*w as f64).powi(2)).sum();
        let mut avg = vec![0.0f64; n / 2];
        let mut frames = 0;
        let mut buf = vec![Complex32::new(0.0, 0.0); n];
        for start in (0..x.len().saturating_sub(n)).step_by(n / 2) {
            for i in 0..n {
                buf[i] = Complex32::new(x[start + i] * win[i], 0.0);
            }
            fft.process(&mut buf);
            for k in 0..n / 2 {
                avg[k] += buf[k].norm_sqr() as f64;
            }
            frames += 1;
        }
        let bin = fs as f64 / n as f64;
        // One-sided power per bin, so a sine of amplitude a sums to a²/2.
        let scale = 2.0 / (frames as f64 * wpow * n as f64);
        let p: Vec<f64> = avg.iter().map(|v| v * scale).collect();
        let band = |lo: f64, hi: f64| ((lo / bin) as usize, (hi / bin) as usize);
        let (a, b) = band(300.0, 2900.0);
        let mut sorted: Vec<f64> = p[a..b].to_vec();
        sorted.sort_by(|x, y| x.total_cmp(y));
        let floor = sorted[sorted.len() / 2];
        let half = (0.5 * shift + 1.5 * baud) as f64;
        let (sa, sb) = band(center as f64 - half, center as f64 + half);
        let sig: f64 = p[sa..=sb].iter().map(|v| v - floor).sum();
        (sig, floor / bin)
    }

    fn decode(x: &[f32], fs: f32, cfg: &RttyConfig) -> (String, RttyStatus) {
        let mut d = RttyDecoder::new(fs, cfg.clone());
        let mut got = Vec::new();
        for c in x.chunks((fs * 0.02) as usize) {
            d.push(c);
            got.extend(d.take());
        }
        d.push(&vec![0.0; (3.0 * fs) as usize]);
        got.extend(d.take());
        let st = d.status();
        (got.iter().map(|t| t.text.as_str()).collect::<Vec<_>>().join("\n"), st)
    }

    /// What the three captures carried, read from the best copies and the
    /// sense of the forecast; the start of each is wherever the capture
    /// began, so it is cut to what any decoder could have heard.
    const LIVE_REF: [(&str, &str); 3] = [
        ("rtty1-iq.wav", "SOUTHEAST AND INCREASING\nSLOWLY 4, SOUTHERN PART LATER LOCALLY 5.\nVISIBILITY/WEATHER: FOG PATCHES.\nSEA: INCREASING"),
        ("rtty2-iq.wav", "METER.\nWESTERN BALTIC:\nWIND: EAST TO NORTHEAST 2 TO 3, INCREASING 4, WESTERN PART LATER\nLOCALLY 5, SHIFTING\nSOUT"),
        ("rtty3-iq.wav", "VISIBILITY/WEATHER: FIRST FOG PATCHES.\nSEA: 0,5 METER.\nSOUTHERN BALTIC:\nWIND: NORTHEASTERLY WINDS 3 TO 4,"),
    ];

    /// The call-up in rtty4 (DWD, 19:15:00 UTC): the RY run, a pause,
    /// CR CR LF LTRS and CQ, which the capture cuts off after "CQ ".
    /// Misframed after the pause, it printed "RYEZ:1 7".
    #[test]
    fn rtty_live_callup() {
        let Ok(dir) = std::env::var("USDR_RTTY_LIVE") else { return };
        let Some((fs, iq)) = read_iq_wav(&std::path::Path::new(&dir).join("rtty4-iq.wav")) else { return };
        let (text, _) = decode(&usb_audio(fs, &iq), fs, &RttyConfig::dwd());
        println!("{text}");
        let ry = text.lines().position(|l| l.len() > 40 && l.starts_with("RYRYRY")).expect("the RY run");
        let next = text.lines().nth(ry + 1).unwrap_or("");
        assert!(text.lines().nth(ry).unwrap().chars().all(|c| c == 'R' || c == 'Y'), "{text}");
        assert!(next.starts_with("CQ "), "{text}");
        assert!(text.contains("FREQUENCIES   4583 KHZ   7646 KHZ   10100.8 KHZ"), "{text}");
    }

    #[test]
    fn rtty_live_recordings() {
        let Ok(dir) = std::env::var("USDR_RTTY_LIVE") else { return };
        let cfg = RttyConfig::dwd();
        let seeds = 4u64;
        println!("capture        as heard    0 dB   -5 dB   (CER % against the transcript, {seeds} noise seeds)");
        for (name, want) in LIVE_REF {
            let Some((fs, iq)) = read_iq_wav(&std::path::Path::new(&dir).join(name)) else {
                println!("{name}: not readable");
                continue;
            };
            let audio = usb_audio(fs, &iq);
            let (clean, st) = decode(&audio, fs, &cfg);
            let (sig, n0) = measure(&audio, fs, st.center_hz, cfg.shift_hz, cfg.baud);
            let snr = 10.0 * (sig / (n0 * 2500.0)).log10();
            let cer = |t: &str| 100.0 * levenshtein(t, want) as f32 / want.len() as f32;
            let mut row = format!("{name} {:>7.1}", cer(&clean));
            let mut shown = Vec::new();
            for target in [0.0f64, -5.0] {
                let mut total = 0.0;
                for seed in 1..=seeds {
                    // Added noise brings the capture to `target` in 2500 Hz.
                    let want_n0 = sig / 10f64.powf(target / 10.0) / 2500.0;
                    let add = (want_n0 - n0).max(0.0);
                    let sigma = (add * fs as f64 / 2.0).sqrt() as f32;
                    let mut rng = Rng::new(seed);
                    let noisy: Vec<f32> = audio.iter().map(|v| v + sigma * rng.gauss()).collect();
                    let (txt, _) = decode(&noisy, fs, &cfg);
                    total += cer(&txt);
                    if seed == 1 {
                        shown.push((target, txt));
                    }
                }
                row += &format!(" {:>7.1}", total / seeds as f32);
            }
            println!("{row}   (heard at {snr:.1} dB, {:.1} Hz, reverse {})", st.center_hz, st.reverse);
            if std::env::var("USDR_RTTY_SHOW").is_ok() {
                println!("{clean}");
                for (t, txt) in shown {
                    println!("-- {t:+.0} dB:\n{txt}");
                }
            }
        }
    }
}
