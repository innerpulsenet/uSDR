//! HF radiofax (WEFAX) receiver: upper-sideband audio in, image rows out.
//!
//! The fax is an FM subcarrier in the audio passband: black at 400 Hz below
//! the carrier, white at 400 Hz above, every shade linearly between. The
//! audio is mixed down by the carrier, cut to the fax's band (narrower as
//! the signal weakens) and taken to 12 kHz. Each pixel is the mean of the
//! instantaneous frequency, limited to the black..white range, over the
//! exact time span of that pixel, so a line of 1809 pixels in half a second
//! is sampled the same way whatever the rate and wherever the pixel edges
//! fall between samples, and a click costs a pixel a third of its range at
//! most.
//!
//! A line clock counts decimated samples as a fraction (`f64`), so a line
//! period that is not a whole number of samples, or one trimmed for a clock
//! error, never drifts: every line starts exactly one period after the last,
//! and every pixel edge is placed from the line start, not from the previous
//! pixel.
//!
//! A transmission opens with the APT start tone (black/white alternating at
//! 300 Hz for IOC 576, 675 Hz for IOC 288) and about 30 s of phasing lines,
//! each a white pulse on 5 % of the line followed by black (WMO practice;
//! the Australian Bureau of Meteorology documents 25 ms white then 475 ms
//! black at 120 LPM). The leading edge of that pulse is where a line begins.
//! It closes with the stop tone (450 Hz) and a few seconds of black. The
//! tones are found by Goertzel filters on the discriminator output, judged
//! over two seconds, so picture content with a regular pattern cannot start
//! or stop an image. The start tone gives the carrier's true center, and the
//! phasing lines give the black and white levels and the sample clock's
//! error.
//!
//! Weak and fading signals shaped most of this: the baseband filter narrows
//! with the SNR; lines carry frequencies rather than gray levels until they
//! are released, so levels measured late still apply; the tone and phasing
//! detectors integrate over seconds and several lines instead of judging
//! one block or one line; and the SNR reading takes medians over 40 ms
//! pieces, so a static crash or a fade does not read as the carrier gone.
//! The test module holds the measurements behind the numbers here: a
//! two-path Watterson channel with impulse noise, a sweep over SNR, and
//! harnesses for real captures and real charts.

use num_complex::Complex32;
use scannerd_dsp::DecimFir;
use std::collections::VecDeque;

/// The rate the discriminator runs at, before rounding to a whole decimation
/// of the input rate. A 120 LPM IOC 576 line is 6000 samples here, 3.3
/// samples a pixel, which keeps the pixel integration exact without wasting
/// work on a band that is only 2.4 kHz wide.
const DEC_RATE_HZ: f32 = 12_000.0;
/// Black and white sit this far either side of the carrier, nominally.
const DEVIATION_HZ: f32 = 400.0;
/// One-sided cutoff of the baseband filter, chosen by the SNR (in 3 kHz,
/// averaged over about 5 s): the widest band whose floor the SNR clears.
/// The deviation is 400 Hz and the finest picture detail (alternate pixels)
/// modulates at 1.8 kHz, so any cutoff trades detail for noise. At 1.2 kHz
/// 2-pixel stripes keep 60 % of their contrast and 3-pixel stripes 82 %;
/// that is the cutoff for a strong signal. Measured on a real DWD chart
/// re-sent through the channel simulator, a fixed 650 Hz raised the
/// picture's correlation with the source at 5 dB from 0.73 to 0.85 and at
/// 0 dB from 0.40 to 0.62, and cost almost nothing above 15 dB but fine
/// detail; the bands keep that detail while the signal can carry it. Every band
/// uses the same tap count, so the delay, and the line alignment, never
/// change. The mixer's image at twice the carrier is far into every
/// stopband.
const BANDS: [(f32, f32); 4] = [(f32::MIN, 650.0), (6.0, 800.0), (10.0, 1_000.0), (15.0, 1_200.0)];
/// A band change needs the SNR this far past the floor it crosses, so a
/// reading near one does not flap between two.
const BAND_HYST_DB: f32 = 1.5;
/// Baseband filter length at a 48 kHz input, scaled with the input rate.
const TAPS_AT_48K: usize = 127;
/// Tone-detector samples are limited to this far from the carrier: a noise
/// click can read as several kHz.
const CLAMP_HZ: f32 = 600.0;
/// The tone detectors run on sums of this many phase products, 3 kHz,
/// which still reaches 675 Hz.
const TONE_DECIM: u32 = 4;
/// Tone analysis block. Every APT tone completes a whole number of cycles
/// in 0.2 s (60, 90, 135), so a constant carrier offset leaks nothing into
/// the tone bins.
const BLOCK_S: f64 = 0.2;
const START_576_HZ: f64 = 300.0;
const STOP_HZ: f64 = 450.0;
const START_288_HZ: f64 = 675.0;
/// Blocks a tone is judged over (two seconds).
const TONE_RING: usize = 10;
/// Over those two seconds, this share of the frequency variance must lie in
/// the tone's bin ...
const TONE_SHARE: f64 = 0.06;
/// ... and this many of the blocks must carry at least `BLOCK_SHARE`, so
/// one loud block cannot pass for two seconds of tone, and a fade that takes
/// a block or two does not reset the count. Over 70 minutes of noise,
/// impulse noise, synthetic pictures and a DWD chart, from 25 dB down to
/// 0 dB in a poor fading channel, the two-second share never passed 1.4 %
/// and no more than two blocks in ten passed 2.5 %. A start tone at -3 dB
/// passes both.
const TONE_HITS: usize = 7;
const BLOCK_SHARE: f64 = 0.025;
/// Blocks without the start tone that end it.
const TONE_END_MISSES: u32 = 2;
/// SNR pieces: short enough that fading is flat across one, so the envelope
/// moments describe noise and not the fade.
const SUB_S: f64 = 0.04;
/// Baseband samples per SNR sample.
const SNR_DECIM: u32 = 6;
/// Lines are held this long before they are released, so the ones that turn
/// out to be the start of a stop tone (it takes two seconds to be sure) can
/// be dropped instead of drawn as noise at the bottom of the chart, and so
/// the first rows can be leveled from what follows them.
const HOLD_S: f64 = 2.6;
/// No chart is this long (a 20-minute chart at 120 LPM is 2400 lines), so a
/// receiver still running here has missed the stop tone.
const MAX_LINES: u32 = 3000;
/// The transmission is over when the SNR over the last `LOSS_S` is below
/// `LOSS_DB`. The moments are averaged over the whole window before the
/// ratio is taken: a 1 s reading on noise alone scatters up to -3 dB, but
/// over 45 s it stays under -9 dB (white noise, or noise through the
/// server's sideband filter), while a picture at -6 dB in 3 kHz reads -6
/// to -3.5 dB whether it fades or not.
const LOSS_DB: f32 = -7.5;
const LOSS_S: f64 = 45.0;
/// Phasing lines that must agree before the line start is trusted.
const PHASING_MIN: u32 = 5;
/// Consecutive non-phasing lines that end phasing. One is not enough: a
/// crash of static or a fade spoils a phasing line now and then.
const PHASING_MISSES: usize = 2;
/// Without phasing this long after the start tone, the station is taken to
/// have skipped it, and the image starts unaligned.
const PHASING_GIVEUP_S: f64 = 15.0;
/// Phasing that goes on this long is taken as done (the norm is 30 s).
const PHASING_CAP_S: f64 = 90.0;
/// How far past black and white a phasing line's samples may read.
const PHASING_MARGIN_HZ: f32 = 200.0;
/// Phasing lines averaged for the final line start and levels.
const PHASING_AVG: usize = 16;
/// Lines averaged to find a phasing pulse not yet located. A pulse 5 % of
/// the line wide stands out of one line at a good SNR; averaging a few
/// finds it at -3 dB, where single lines are mostly noise.
const PHASING_ACQ: usize = 4;
/// Acquisition: the averaged pulse must differ from the rest of the line by
/// this share of full scale and by `ACQ_Z` standard errors of the rest's
/// scatter, with the rest within `ACQ_REST` of its level. Noise limited to
/// black..white reads 0.35 or so either side of the middle at -3 dB, which
/// lifts the black and pulls the white in; averaged noise lines peak near
/// z = 4 somewhere along 1809 positions, a real pulse at -3 dB near 10.
const ACQ_CONTRAST: f32 = 0.2;
const ACQ_Z: f32 = 8.0;
const ACQ_REST: f32 = 0.5;
/// Once located, a single line is a phasing line when its pulse, within the
/// position tolerance, stands out this far (share of full scale, and
/// standard errors: only 37 positions are searched, so noise rarely passes
/// z = 4).
const LINE_CONTRAST: f32 = 0.15;
const LINE_Z: f32 = 4.0;
/// How far from 1 the pulse and rest levels may sum (see `Fit::balanced`).
const BALANCE: f32 = 0.25;
/// Lines in a row without the pulse that drop a lock not yet established.
const LOCK_MISSES: u32 = 3;
/// Phasing lines needed for a slant estimate, and the most it may correct.
const SLANT_MIN_LINES: usize = 12;
const SLANT_MAX_PPM: f64 = 1000.0;

/// An APT control tone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AptTone {
    /// Start tone for IOC 576 (300 Hz alternation).
    Start576,
    /// Start tone for IOC 288 (675 Hz alternation).
    Start288,
    /// Stop tone (450 Hz alternation).
    Stop,
}

const TONES: [AptTone; 3] = [AptTone::Start576, AptTone::Stop, AptTone::Start288];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WefaxState {
    /// Waiting for a start tone (or `start_now`).
    Idle,
    /// A start tone has been heard and is still on.
    Starting,
    /// The start tone has ended; the line start is being found from the
    /// phasing lines.
    Phasing,
    /// Image rows are being produced; `line` of them have been released.
    Receiving { line: u32 },
}

#[derive(Clone, Debug, PartialEq)]
pub enum WefaxEvent {
    /// An image has begun, on an APT start tone or `start_now`.
    Start { ioc: u16, lpm: f32 },
    /// The line start was found from the phasing lines: column `shift_px`
    /// of the free-running line clock is where each row now begins.
    Phased { shift_px: i32 },
    /// One image row, `width()` pixels, 0 = black .. 255 = white.
    Line(Vec<u8>),
    /// The image has ended (stop tone, `stop_now`, a long carrier loss, or
    /// the line cap) after `lines` rows.
    Stop { lines: u32 },
}

/// Where the black and white levels came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LevelSource {
    /// ±400 Hz about `carrier_hz`.
    Nominal,
    /// ±400 Hz about the center of the start tone.
    StartTone,
    /// Black and white measured on the phasing lines.
    Phasing,
    /// White taken from the picture's dominant light level (a chart's
    /// paper), black 800 Hz below it: used after `start_now`, when there was
    /// nothing else to measure.
    Picture,
}

/// Shapes a phasing line can take. The norm is a white pulse on 5 % of the
/// line; some stations invert it. Each is fitted the same way: a window of
/// the pulse's width slid round the line. ISO 9876 also allows a symmetric
/// form, half white and half black, but no station in use sends it, and a
/// picture that is darker on one side than the other fits it: a gradient
/// was taken for phasing and swallowed whole, so it is not looked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pulse {
    White5,
    Black5,
}

impl Pulse {
    const ALL: [Pulse; 2] = [Pulse::White5, Pulse::Black5];

    fn len(self, width: usize) -> usize {
        ((width as f32 * 0.05).round() as usize).max(1)
    }

    fn white(self) -> bool {
        !matches!(self, Pulse::Black5)
    }

    /// The inverted pulse is a dark band on white, and white paper with a
    /// dark band down it is an ordinary chart (a coast, a legend box); a
    /// real chart at 5 dB passed the weak-signal limits for it. So it must
    /// stand out as only a strong signal lets it: inverted phasing is found
    /// above about 3 dB, the normal one down to -3 dB. A black page with a
    /// white band is not a chart, so the normal pulse keeps the low limit.
    fn min_contrast(self, weak: f32) -> f32 {
        if self.white() { weak } else { weak.max(0.6) }
    }
}

#[derive(Clone, Copy, Debug)]
struct Fit {
    /// Column where the pulse begins, to a fraction of a pixel.
    pos: f64,
    /// Mean level under the pulse and over the rest, 0 = black .. 1 = white.
    inside: f32,
    rest: f32,
    /// The contrast in standard errors of the rest's scatter.
    z: f32,
    /// The pulse is no wider than the template's.
    narrow: bool,
    /// The rest of the line is flat to within its scatter.
    flat: bool,
}

impl Fit {
    /// Pulse level minus the rest (the other way for a black pulse).
    fn contrast(&self, pulse: Pulse) -> f32 {
        if pulse.white() { self.inside - self.rest } else { self.rest - self.inside }
    }

    /// The pulse and the rest sit at opposite extremes. Noise pulls both
    /// toward the middle by the same amount, so they stay balanced about it
    /// however weak the signal; a gray block on white paper does not.
    fn balanced(&self) -> bool {
        (self.inside + self.rest - 1.0).abs() <= BALANCE
    }

    /// Good enough to say an averaged line holds a phasing pulse somewhere.
    fn acquires(&self, pulse: Pulse) -> bool {
        let rest_ok = if pulse.white() { self.rest <= ACQ_REST } else { self.rest >= 1.0 - ACQ_REST };
        self.contrast(pulse) >= pulse.min_contrast(ACQ_CONTRAST)
            && self.z >= ACQ_Z
            && rest_ok
            && self.narrow
            && self.flat
            && self.balanced()
    }

    /// Good enough to say a single line shows the pulse where it was.
    fn confirms(&self, pulse: Pulse) -> bool {
        self.contrast(pulse) >= pulse.min_contrast(LINE_CONTRAST)
            && self.z >= LINE_Z
            && self.narrow
            && self.flat
            && self.balanced()
    }
}

/// Fit `line` (levels 0..1) to the ideal phasing line of shape `pulse`,
/// searching starts within `tol` of `near` (or everywhere). Against a
/// two-level template the error depends only on the sum under the pulse, so
/// one pass of prefix sums finds the best position.
fn fit(line: &[f32], pulse: Pulse, near: Option<(usize, usize)>) -> Fit {
    let w = line.len();
    let d = pulse.len(w).min(w.saturating_sub(1)).max(1);
    let mut prefix = Vec::with_capacity(w + d + 1);
    let mut acc = 0f64;
    prefix.push(0.0);
    for i in 0..w + d {
        acc += line[i % w] as f64;
        prefix.push(acc);
    }
    let total = prefix[w];
    let sum_at = |p: usize| {
        let p = p % w;
        prefix[p + d] - prefix[p]
    };
    let sign = if pulse.white() { 1.0 } else { -1.0 };
    let (lo, n) = match near {
        Some((c, tol)) => (c + w - tol.min(w / 2), (2 * tol + 1).min(w)),
        None => (0, w),
    };
    let mut best_p = lo % w;
    let mut best = f64::MIN;
    for k in 0..n {
        let p = (lo + k) % w;
        let s = sign * sum_at(p);
        if s > best {
            best = s;
            best_p = p;
        }
    }
    // The window sum peaks in a triangle as the window slides over the
    // pulse; the slopes either side place the edge between pixels.
    let (sm, s0, sp) = (sign * sum_at(best_p + w - 1), best, sign * sum_at(best_p + 1));
    let drop = s0 - sm.min(sp);
    let frac = if drop > 1e-9 { ((sp - sm) / (2.0 * drop)).clamp(-0.5, 0.5) } else { 0.0 };
    let inside = sign * best;
    // How far the pulse stands out of the rest's own scatter, in standard
    // errors of a window mean: noise and picture content both scatter.
    let sq_in: f64 = (0..d).map(|i| (line[(best_p + i) % w] as f64).powi(2)).sum();
    let sq_total: f64 = line.iter().map(|&v| (v as f64).powi(2)).sum();
    let rest_n = (w - d) as f64;
    let rest = (total - inside) / rest_n;
    let var = ((sq_total - sq_in) / rest_n - rest * rest).max(1e-6);
    let contrast = sign * (inside / d as f64 - rest);
    // The pulse's own width: the windows either side of it are at the
    // rest's level, where a wider block of the picture would carry on.
    let (after, before) = (sum_at(best_p + d) / d as f64, sum_at(best_p + w - d) / d as f64);
    let beside = if pulse.white() { after.max(before) } else { after.min(before) };
    // The rest is flat: sixteen pieces of it, each within its own scatter
    // of the rest's level (about three pixels to an independent sample).
    // Noise scatters; a picture has shapes.
    let span = w.saturating_sub(3 * d);
    let seg = (span / 16).max(1);
    let mut lumpy = 0f64;
    for k in 0..span / seg {
        let a = best_p + 2 * d + k * seg;
        let m = (0..seg).map(|i| line[(a + i) % w] as f64).sum::<f64>() / seg as f64;
        lumpy = lumpy.max((m - rest).abs());
    }
    let seg_se = (var / (seg as f64 / 3.0)).sqrt();
    Fit {
        pos: best_p as f64 + frac,
        inside: (inside / d as f64) as f32,
        rest: rest as f32,
        z: (contrast / (var / d as f64).sqrt()) as f32,
        narrow: sign * (beside - rest) <= 0.3 * contrast,
        flat: lumpy <= 0.15 + 3.0 * seg_se,
    }
}

/// Position tolerance between phasing lines: a percent of the line, which
/// also absorbs an uncorrected clock error of a few hundred ppm.
fn tolerance(width: usize) -> usize {
    (width / 100).max(4)
}

/// SNR, dB in the measuring band, from smoothed signal power (signed) and
/// total power.
fn snr_of(sig: f32, m2: f32) -> Option<f32> {
    if m2 <= 1e-30 {
        return None;
    }
    let sig = sig.max(0.0);
    let noise = m2 - sig;
    Some(if noise <= 0.0 { 50.0 } else { 10.0 * (sig / noise).max(1e-9).log10() })
}

fn median(v: &mut [f32]) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    let mid = v.len() / 2;
    *v.select_nth_unstable_by(mid, f32::total_cmp).1
}

/// Real-input mixer. The audio is real, so each sample costs two multiplies
/// against the phasor rather than a full complex product.
struct Mixer {
    phasor: Complex32,
    step: Complex32,
    since_norm: u32,
}

impl Mixer {
    fn new(hz: f32, fs: f32) -> Self {
        let mut m = Self { phasor: Complex32::new(1.0, 0.0), step: Complex32::new(1.0, 0.0), since_norm: 0 };
        m.set_freq(hz, fs);
        m
    }

    /// Change the rate and keep the phase, so a carrier trim does not click.
    fn set_freq(&mut self, hz: f32, fs: f32) {
        let w = -std::f64::consts::TAU * hz as f64 / fs as f64;
        self.step = Complex32::new(w.cos() as f32, w.sin() as f32);
    }

    fn mix(&mut self, input: &[f32], out: &mut Vec<Complex32>) {
        out.clear();
        out.reserve(input.len());
        for &x in input {
            out.push(self.phasor * x);
            self.phasor *= self.step;
            self.since_norm += 1;
            if self.since_norm >= 1024 {
                self.since_norm = 0;
                // Newton step toward |phasor| = 1, cheaper than a square root.
                self.phasor *= 1.5 - 0.5 * self.phasor.norm_sqr();
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Goertzel {
    coeff: f64,
    s1: f64,
    s2: f64,
}

impl Goertzel {
    fn new(hz: f64, fs: f64) -> Self {
        Self { coeff: 2.0 * (std::f64::consts::TAU * hz / fs).cos(), s1: 0.0, s2: 0.0 }
    }

    #[inline]
    fn update(&mut self, x: f64) {
        let s0 = x + self.coeff * self.s1 - self.s2;
        self.s2 = self.s1;
        self.s1 = s0;
    }

    /// |X(f)|² over the block, and ready for the next.
    fn take_power(&mut self) -> f64 {
        let p = self.s1 * self.s1 + self.s2 * self.s2 - self.coeff * self.s1 * self.s2;
        self.s1 = 0.0;
        self.s2 = 0.0;
        p
    }
}

#[derive(Clone, Copy)]
struct Block {
    /// Decimated sample index where the block began.
    start: u64,
    /// Mean discriminator frequency over the block, Hz from the carrier.
    mean_hz: f64,
    /// Frequency variance, Hz².
    var: f64,
    /// Power in each tone's bin (`TONES` order), Hz², comparable to `var`.
    power: [f64; 3],
}

impl Block {
    fn share(&self, k: usize) -> f64 {
        if self.var > 0.0 { self.power[k] / self.var } else { 0.0 }
    }
}

/// Where black and white sit, Hz from the carrier.
#[derive(Clone, Copy, Debug)]
struct Levels {
    black_hz: f32,
    white_hz: f32,
}

impl Levels {
    fn centered(center: f32) -> Self {
        Levels { black_hz: center - DEVIATION_HZ, white_hz: center + DEVIATION_HZ }
    }

    #[inline]
    fn unit(&self, hz: f32) -> f32 {
        ((hz - self.black_hz) / (self.white_hz - self.black_hz)).clamp(0.0, 1.0)
    }

    fn pixels(&self, hz: &[f32]) -> Vec<u8> {
        let k = 255.0 / (self.white_hz - self.black_hz);
        hz.iter().map(|&f| ((f - self.black_hz) * k).round().clamp(0.0, 255.0) as u8).collect()
    }

    fn units(&self, hz: &[f32]) -> Vec<f32> {
        hz.iter().map(|&f| self.unit(f)).collect()
    }
}

/// Turns the discriminator's phase products into raw lines of pixel
/// frequencies (Hz from the carrier) on a free-running fractional clock.
///
/// Each pixel is the time-weighted mean, over its span, of the per-sample
/// frequency limited to the black..white range. The limit is what makes a
/// click harmless: a phase slip reads as kHz for a sample or two, and
/// unlimited it would blacken or whiten the pixel. Summing the phase
/// products instead (which weights each sample by its amplitude, so the
/// samples near zero that slip count for little) was measured too, on
/// synthetic and real charts from 15 to 0 dB with and without fading; it
/// came out 1–2 % worse in correlation everywhere, so the limiter stays.
struct LineClock {
    width: usize,
    /// Decimated samples per line, fractional and slant-trimmed.
    spl: f64,
    /// Time the current line begins; samples before it are skipped.
    line_start: f64,
    /// Pixel being accumulated.
    j: usize,
    px_end: f64,
    acc: f32,
    wsum: f32,
    buf: Vec<f32>,
}

impl LineClock {
    fn new(width: usize, spl: f64) -> Self {
        Self {
            width,
            spl,
            line_start: 0.0,
            j: 0,
            px_end: 0.0,
            acc: 0.0,
            wsum: 0.0,
            buf: Vec::with_capacity(width),
        }
    }

    fn pixel_end(&self) -> f64 {
        self.line_start + (self.j + 1) as f64 * self.spl / self.width as f64
    }

    fn restart(&mut self, at: f64) {
        self.line_start = at;
        self.j = 0;
        self.acc = 0.0;
        self.wsum = 0.0;
        self.buf.clear();
        self.px_end = self.pixel_end();
    }

    /// New geometry. The partial line is dropped and the clock carries on
    /// from the next line boundary, so a width or rate change mid-image does
    /// not throw away the line phase.
    fn reconfigure(&mut self, width: usize, spl: f64, now: f64) {
        let next = self.line_start + spl;
        self.width = width;
        self.spl = spl;
        self.restart(if next > now { next } else { now });
    }

    /// A slant trim keeps the line underway; only its remaining pixel edges
    /// move.
    fn set_spl(&mut self, spl: f64) {
        self.spl = spl;
        self.px_end = self.pixel_end();
    }

    /// Integrate one phase product covering `[t, t + 1)`. A finished line
    /// is pushed to `out`.
    #[inline]
    fn push(&mut self, t: f64, d: Complex32, hz_per_rad: f32, lim: (f32, f32), out: &mut Vec<Vec<f32>>) {
        let s1 = t + 1.0;
        if s1 <= self.line_start {
            return;
        }
        let mut s0 = t.max(self.line_start);
        let v = (d.im.atan2(d.re) * hz_per_rad).clamp(lim.0, lim.1);
        while s1 >= self.px_end {
            let w = (self.px_end - s0) as f32;
            if w > 0.0 {
                self.acc += v * w;
                self.wsum += w;
            }
            self.buf.push(if self.wsum > 0.0 { self.acc / self.wsum } else { v });
            self.acc = 0.0;
            self.wsum = 0.0;
            s0 = self.px_end;
            self.j += 1;
            if self.j == self.width {
                out.push(std::mem::replace(&mut self.buf, Vec::with_capacity(self.width)));
                self.line_start += self.spl;
                self.j = 0;
            }
            self.px_end = self.pixel_end();
        }
        let w = (s1 - s0) as f32;
        self.acc += v * w;
        self.wsum += w;
    }
}

/// The search for the line start, from the end of the start tone. Lines
/// here are in Hz.
#[derive(Default)]
struct PhaseSearch {
    /// Raw lines seen since phasing began.
    lines: u32,
    /// The last few lines, for acquiring a pulse not yet located.
    acq: VecDeque<Vec<f32>>,
    /// The pulse shape and where it is, once located.
    lock: Option<(Pulse, f64)>,
    /// Lines that showed the pulse at the locked place since it was found.
    count: u32,
    /// Lines in a row that did not, while the lock is young.
    run_miss: u32,
    /// The most recent phasing lines, averaged for the final position and
    /// the levels.
    recent: VecDeque<Vec<f32>>,
    /// (line number, pulse position unwrapped) of each phasing line, for
    /// the slant.
    track: Vec<(f64, f64)>,
    /// Non-phasing lines since the last phasing line, once phasing is
    /// established: the first rows of the image if phasing has ended.
    misses: Vec<Vec<f32>>,
    /// Every line before phasing was established, drawn unaligned if it
    /// never is.
    early: Vec<Vec<f32>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Idle,
    Starting,
    Phasing,
    Receiving,
}

pub struct WefaxDecoder {
    fs: f32,
    fs_dec: f64,
    lpm: f32,
    ioc: u16,
    carrier_hz: f32,
    auto_start: bool,
    slant_ppm: f64,
    /// Clock error measured on the phasing lines, added to `slant_ppm`.
    auto_slant_ppm: f64,

    mixer: Mixer,
    fir: DecimFir,
    /// The baseband filter's band (`BANDS`), and the slow SNR that picks it.
    band: usize,
    slow: Option<(f32, f32)>,
    mixed: Vec<Complex32>,
    base: Vec<Complex32>,
    prev: Complex32,
    /// Decimated samples processed.
    n: u64,
    /// Radians per sample to Hz.
    hz_per_rad: f32,

    tones: [Goertzel; 3],
    tone_acc: Complex32,
    tone_k: u32,
    blk_len: u32,
    blk_n: u32,
    blk_tn: u32,
    blk_start: u64,
    blk_sum: f64,
    blk_sq: f64,
    ring: VecDeque<Block>,
    heard: Option<AptTone>,
    tone_misses: u32,
    /// The start tone that opened this image, for measuring its center.
    opening: Option<AptTone>,
    off_sum: f64,
    off_n: u32,

    snr_k: u32,
    sub_len: u32,
    sub_n: u32,
    sub_m2: f64,
    sub_m4: f64,
    /// Per-piece P² and total power estimates for the current block.
    sub_s: Vec<f32>,
    sub_noise: Vec<f32>,
    /// Smoothed signal power (signed) and total power.
    sig_s: f32,
    noise_s: f32,
    snr_db: Option<f32>,
    /// Each block's P² and total power over the last `LOSS_S`, and their
    /// sums.
    loss: VecDeque<(f64, f64)>,
    loss_sum: (f64, f64),

    levels: Levels,
    level_source: LevelSource,

    mode: Mode,
    clock: LineClock,
    finished: Vec<Vec<f32>>,
    phase: PhaseSearch,
    shift: Option<usize>,
    pulse: Option<Pulse>,
    prev_raw: Option<Vec<f32>>,
    /// Still dropping aligned rows that are phasing lines.
    suppress: bool,
    /// Rows waiting out `HOLD_S`, in Hz, with the time each ended.
    held: VecDeque<(u64, Vec<f32>)>,
    emitted: u32,
    events: Vec<WefaxEvent>,
}

impl WefaxDecoder {
    /// A receiver for 120 LPM, IOC 576, carrier at 1900 Hz, starting on its
    /// own when it hears a start tone.
    pub fn new(fs: f32) -> Self {
        let decim = ((fs / DEC_RATE_HZ).round() as usize).max(1);
        let fs_dec = fs as f64 / decim as f64;
        let ntaps = ((TAPS_AT_48K as f32 * fs / 48_000.0).round() as usize).max(15) | 1;
        let blk_len = (BLOCK_S * fs_dec).round().max(16.0) as u32;
        let tone_fs = fs_dec / TONE_DECIM as f64;
        let mut this = Self {
            fs,
            fs_dec,
            lpm: 120.0,
            ioc: 576,
            carrier_hz: 1900.0,
            auto_start: true,
            slant_ppm: 0.0,
            auto_slant_ppm: 0.0,
            mixer: Mixer::new(1900.0, fs),
            fir: DecimFir::new(BANDS[BANDS.len() - 1].1, fs, decim, ntaps),
            band: BANDS.len() - 1,
            slow: None,
            mixed: Vec::new(),
            base: Vec::new(),
            prev: Complex32::new(0.0, 0.0),
            n: 0,
            hz_per_rad: (fs_dec / std::f64::consts::TAU) as f32,
            tones: [
                Goertzel::new(START_576_HZ, tone_fs),
                Goertzel::new(STOP_HZ, tone_fs),
                Goertzel::new(START_288_HZ, tone_fs),
            ],
            tone_acc: Complex32::new(0.0, 0.0),
            tone_k: 0,
            blk_len,
            blk_n: 0,
            blk_tn: 0,
            blk_start: 0,
            blk_sum: 0.0,
            blk_sq: 0.0,
            ring: VecDeque::with_capacity(TONE_RING + 1),
            heard: None,
            tone_misses: 0,
            opening: None,
            off_sum: 0.0,
            off_n: 0,
            snr_k: 0,
            sub_len: (SUB_S * fs_dec / SNR_DECIM as f64).round().max(16.0) as u32,
            sub_n: 0,
            sub_m2: 0.0,
            sub_m4: 0.0,
            sub_s: Vec::with_capacity(32),
            sub_noise: Vec::with_capacity(32),
            sig_s: 0.0,
            noise_s: 0.0,
            snr_db: None,
            loss: VecDeque::new(),
            loss_sum: (0.0, 0.0),
            levels: Levels::centered(0.0),
            level_source: LevelSource::Nominal,
            mode: Mode::Idle,
            clock: LineClock::new(1809, 6000.0),
            finished: Vec::new(),
            phase: PhaseSearch::default(),
            shift: None,
            pulse: None,
            prev_raw: None,
            suppress: false,
            held: VecDeque::new(),
            emitted: 0,
            events: Vec::new(),
        };
        this.clock = LineClock::new(this.width(), this.samples_per_line());
        this
    }

    fn samples_per_line(&self) -> f64 {
        self.fs_dec * 60.0 / self.lpm as f64 * (1.0 + (self.slant_ppm + self.auto_slant_ppm) * 1e-6)
    }

    fn reconfigure_clock(&mut self) {
        let (w, spl) = (self.width(), self.samples_per_line());
        self.clock.reconfigure(w, spl, self.n as f64);
        self.prev_raw = None;
        if self.mode == Mode::Phasing {
            self.phase = PhaseSearch::default();
        }
    }

    pub fn set_lpm(&mut self, lpm: f32) {
        let lpm = lpm.clamp(10.0, 600.0);
        if lpm != self.lpm {
            self.lpm = lpm;
            self.reconfigure_clock();
        }
    }

    pub fn set_ioc(&mut self, ioc: u16) {
        let ioc = ioc.clamp(100, 1200);
        if ioc != self.ioc {
            let old = self.width();
            self.ioc = ioc;
            // The line start is a time within the line, so it scales with
            // the width rather than being lost.
            if let Some(p) = self.shift {
                self.shift = Some(p * self.width() / old.max(1) % self.width());
            }
            self.reconfigure_clock();
        }
    }

    pub fn set_carrier_hz(&mut self, hz: f32) {
        self.carrier_hz = hz;
        self.mixer.set_freq(hz, self.fs);
    }

    pub fn set_auto_start(&mut self, on: bool) {
        self.auto_start = on;
    }

    /// Trim the line period for a sample clock that runs `ppm` fast
    /// (positive: more samples per real second, so each line spans more).
    /// It adds to what the phasing lines measured.
    pub fn set_slant_ppm(&mut self, ppm: f64) {
        self.slant_ppm = ppm.clamp(-5_000.0, 5_000.0);
        let spl = self.samples_per_line();
        self.clock.set_spl(spl);
    }

    /// The clock error measured on the last image's phasing lines and taken
    /// out of its line period, ppm (0 until phasing gives one).
    pub fn auto_slant_ppm(&self) -> f64 {
        self.auto_slant_ppm
    }

    pub fn lpm(&self) -> f32 {
        self.lpm
    }

    pub fn ioc(&self) -> u16 {
        self.ioc
    }

    /// Pixels per line: IOC × π, truncated (1809 for 576, 904 for 288), the
    /// count the fax programs in common use agree on.
    pub fn width(&self) -> usize {
        (self.ioc as f64 * std::f64::consts::PI) as usize
    }

    pub fn state(&self) -> WefaxState {
        match self.mode {
            Mode::Idle => WefaxState::Idle,
            Mode::Starting => WefaxState::Starting,
            Mode::Phasing => WefaxState::Phasing,
            Mode::Receiving => WefaxState::Receiving { line: self.emitted },
        }
    }

    /// Carrier-to-noise ratio, dB, referred to 3 kHz of noise, from
    /// the second and fourth moments of the baseband envelope (an FM signal
    /// has a constant one, noise does not), taken over 40 ms pieces and the
    /// median of each 0.2 s, so neither fading nor a static crash reads as
    /// noise. Independent of the audio level; `None` until a block of audio
    /// has been heard, or on digital silence.
    pub fn carrier_snr_db(&self) -> Option<f32> {
        self.snr_db
    }

    /// The APT tone heard in the last 0.2 s, if any, before it has persisted
    /// long enough to act on.
    pub fn tone(&self) -> Option<AptTone> {
        self.heard
    }

    /// Where the picture's center frequency sits against `carrier_hz`, Hz
    /// (positive: the station is above). Measured on the start tone, the
    /// phasing lines or the picture; taken out of the pixel levels.
    pub fn carrier_offset_hz(&self) -> f32 {
        (self.levels.black_hz + self.levels.white_hz) / 2.0
    }

    /// Black and white, Hz from `carrier_hz`, as the current picture is
    /// leveled.
    pub fn levels_hz(&self) -> (f32, f32) {
        (self.levels.black_hz, self.levels.white_hz)
    }

    pub fn level_source(&self) -> LevelSource {
        self.level_source
    }

    /// The column of the line clock where rows begin, once phased.
    pub fn shift_px(&self) -> Option<i32> {
        self.shift.map(|p| p as i32)
    }

    /// Begin an image now, without APT or phasing: rows start wherever the
    /// line clock happens to be.
    pub fn start_now(&mut self) {
        if self.mode != Mode::Idle {
            self.finish(None);
        }
        self.levels = Levels::centered(0.0);
        self.level_source = LevelSource::Nominal;
        self.begin(Mode::Receiving);
        self.shift = Some(0);
        self.clock.restart(self.n as f64);
    }

    pub fn stop_now(&mut self) {
        if self.mode != Mode::Idle {
            self.finish(None);
        }
    }

    pub fn take_events(&mut self) -> Vec<WefaxEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn push(&mut self, audio: &[f32]) {
        let mut mixed = std::mem::take(&mut self.mixed);
        let mut base = std::mem::take(&mut self.base);
        self.mixer.mix(audio, &mut mixed);
        self.fir.process(&mixed, &mut base);
        for &z in &base {
            // Every sixth sample, 2 kHz: far enough apart for the 2.4 kHz
            // passband to leave them nearly independent, which the bias
            // correction in `end_piece` assumes.
            self.snr_k += 1;
            if self.snr_k == SNR_DECIM {
                self.snr_k = 0;
                let p = z.norm_sqr() as f64;
                self.sub_m2 += p;
                self.sub_m4 += p * p;
                self.sub_n += 1;
                if self.sub_n >= self.sub_len {
                    self.end_piece();
                }
            }
            let d = z * self.prev.conj();
            self.prev = z;
            // The tone filters run at a quarter of the rate on the sum of
            // four products, which suppresses clicks the way the pixels do.
            self.tone_acc += d;
            self.tone_k += 1;
            if self.tone_k == TONE_DECIM {
                let fd = ((self.tone_acc.im.atan2(self.tone_acc.re) * self.hz_per_rad) as f64)
                    .clamp(-CLAMP_HZ as f64, CLAMP_HZ as f64);
                for g in &mut self.tones {
                    g.update(fd);
                }
                self.blk_sum += fd;
                self.blk_sq += fd * fd;
                self.blk_tn += 1;
                self.tone_acc = Complex32::new(0.0, 0.0);
                self.tone_k = 0;
            }
            self.blk_n += 1;
            if matches!(self.mode, Mode::Phasing | Mode::Receiving) {
                // Phasing lines, and the first rows of a picture begun with
                // nothing measured, are measured for the levels, so they are
                // limited loosely: a limit at the levels themselves would pull
                // a noisy white or black inward and the measure with it.
                let measuring = self.mode == Mode::Phasing
                    || (self.level_source == LevelSource::Nominal && self.emitted == 0);
                let m = if measuring { PHASING_MARGIN_HZ } else { 0.0 };
                let lim = (self.levels.black_hz - m, self.levels.white_hz + m);
                self.clock.push(self.n as f64, d, self.hz_per_rad, lim, &mut self.finished);
                if !self.finished.is_empty() {
                    let mut lines = std::mem::take(&mut self.finished);
                    for line in lines.drain(..) {
                        self.on_raw_line(line);
                    }
                    self.finished = lines;
                }
            }
            self.n += 1;
            if self.blk_n >= self.blk_len {
                self.on_block();
            }
        }
        self.mixed = mixed;
        self.base = base;
    }

    /// One 40 ms piece of envelope moments: for a constant envelope of power
    /// P in complex Gaussian noise N, `2·M2² - M4 = P²` and `M2 = P + N`.
    /// `M2²` is taken over distinct pairs of samples: the square of the
    /// sample mean overstates it by `M4 / n`, which over 48 samples read pure
    /// noise as -7 dB of signal.
    fn end_piece(&mut self) {
        let n = self.sub_n as f64;
        let (s1, s2) = (self.sub_m2, self.sub_m4);
        let m2 = s1 / n;
        let m2_sq = (s1 * s1 - s2) / (n * (n - 1.0)).max(1.0);
        let m4 = s2 / n;
        // P² is kept signed: on noise alone it scatters about zero, and its
        // median stays there, where clipping each piece at zero first would
        // read every noise piece as a little signal.
        if self.sub_s.len() < 64 {
            self.sub_s.push((2.0 * m2_sq - m4) as f32);
            self.sub_noise.push(m2 as f32);
        }
        self.sub_n = 0;
        self.sub_m2 = 0.0;
        self.sub_m4 = 0.0;
    }

    fn on_block(&mut self) {
        let n = self.blk_tn.max(1) as f64;
        let mean = self.blk_sum / n;
        let var = (self.blk_sq / n - mean * mean).max(0.0);
        let mut power = [0.0; 3];
        for (p, g) in power.iter_mut().zip(&mut self.tones) {
            *p = 2.0 * g.take_power() / (n * n);
        }
        let block = Block { start: self.blk_start, mean_hz: mean, var, power };
        self.heard = (0..3)
            .filter(|&k| block.share(k) > BLOCK_SHARE)
            .max_by(|&a, &b| block.share(a).total_cmp(&block.share(b)))
            .map(|k| TONES[k]);
        if self.ring.len() == TONE_RING {
            self.ring.pop_front();
        }
        self.ring.push_back(block);

        // Medians of the pieces: a crash spoils a piece or two, not the
        // reading. Signal and noise are smoothed apart so a fade lowers the
        // reading only as far as it lowers the signal.
        if !self.sub_s.is_empty() {
            // P stays signed through the smoothing too (the root of P²
            // keeping its sign), so on noise the scatter averages away
            // instead of each block's positive half adding up to a phantom
            // signal. Smoothing P rather than P² keeps it in step with the
            // total power when the signal goes: P² lingers, and its root
            // would outlast the total and read the silence as 50 dB.
            let p2 = median(&mut self.sub_s);
            let m2 = median(&mut self.sub_noise);
            let span = (LOSS_S / BLOCK_S) as usize;
            if self.loss.len() == span
                && let Some((a, b)) = self.loss.pop_front()
            {
                self.loss_sum.0 -= a;
                self.loss_sum.1 -= b;
            }
            self.loss.push_back((p2 as f64, m2 as f64));
            self.loss_sum.0 += p2 as f64;
            self.loss_sum.1 += m2 as f64;
            let p2 = p2.signum() * p2.abs().sqrt();
            if self.snr_db.is_none() {
                (self.sig_s, self.noise_s) = (p2, m2);
            } else {
                self.sig_s += 0.3 * (p2 - self.sig_s);
                self.noise_s += 0.3 * (m2 - self.noise_s);
            }
            self.sub_s.clear();
            self.sub_noise.clear();
            let to_3k = self.to_3k_db();
            self.snr_db = snr_of(self.sig_s, self.noise_s).map(|s| (s + to_3k).clamp(-10.0, 50.0));
            let slow = self.slow.get_or_insert((p2, m2));
            slow.0 += 0.04 * (p2 - slow.0);
            slow.1 += 0.04 * (m2 - slow.1);
            if let Some(snr) = snr_of(slow.0, slow.1).map(|s| s + to_3k) {
                self.pick_band(snr);
            }
        }

        self.blk_n = 0;
        self.blk_tn = 0;
        self.blk_sum = 0.0;
        self.blk_sq = 0.0;
        self.blk_start = self.n;

        let persisted = self.persisted();
        match self.mode {
            Mode::Idle => {
                if let Some((tone @ (AptTone::Start576 | AptTone::Start288), _)) = persisted
                    && self.auto_start
                {
                    self.start_on(tone);
                }
            }
            Mode::Starting => {
                let k = TONES.iter().position(|&t| Some(t) == self.opening).unwrap_or(0);
                if let Some((AptTone::Stop, onset)) = persisted {
                    self.finish(Some(onset));
                } else if block.share(k) > BLOCK_SHARE {
                    self.tone_misses = 0;
                    self.off_sum += mean;
                    self.off_n += 1;
                    self.set_tone_center();
                } else {
                    self.tone_misses += 1;
                    if self.tone_misses >= TONE_END_MISSES {
                        self.mode = Mode::Phasing;
                        self.phase = PhaseSearch::default();
                        self.ring.clear();
                        self.clock.restart(self.n as f64);
                    }
                }
            }
            Mode::Phasing | Mode::Receiving => match persisted {
                Some((AptTone::Stop, onset)) => self.finish(Some(onset)),
                Some((tone, _)) if self.mode == Mode::Phasing && Some(tone) == self.opening => {
                    // The opening tone was only interrupted (a fade or a
                    // crash took two blocks): it is the same start, not a
                    // new image.
                    self.mode = Mode::Starting;
                    self.tone_misses = 0;
                    self.phase = PhaseSearch::default();
                    self.ring.clear();
                }
                Some((tone, onset)) if self.auto_start => {
                    // A new transmission without a stop tone for the last.
                    self.finish(Some(onset));
                    self.start_on(tone);
                }
                _ => {}
            },
        }

        if self.mode != Mode::Idle && self.loss.len() == (LOSS_S / BLOCK_S) as usize {
            let k = self.loss.len() as f64;
            let (p2, m2) = (self.loss_sum.0 / k, self.loss_sum.1 / k);
            let sig = p2.max(0.0).sqrt();
            let dead = m2 <= 0.0 || sig <= 0.0 || self.long_snr_db().is_none_or(|s| s < LOSS_DB as f64);
            if dead {
                // The rows still held are noise.
                self.finish(Some(0));
            }
        }
    }

    /// The SNR the carrier-loss rule judges, over the last `LOSS_S`, in 3 kHz.
    fn long_snr_db(&self) -> Option<f64> {
        let k = self.loss.len() as f64;
        if k == 0.0 {
            return None;
        }
        let (p2, m2) = (self.loss_sum.0 / k, self.loss_sum.1 / k);
        let sig = p2.max(0.0).sqrt();
        Some(10.0 * (sig.max(1e-30) / (m2 - sig).max(1e-30)).log10() + self.to_3k_db() as f64)
    }

    /// Noise in the baseband filter's band against noise in 3 kHz, dB: the
    /// readings are referred to 3 kHz so they do not move when the band does.
    fn to_3k_db(&self) -> f32 {
        10.0 * (2.0 * BANDS[self.band].1 / 3000.0).log10()
    }

    /// Move the baseband filter to the band the slow SNR calls for.
    fn pick_band(&mut self, snr_3k: f32) {
        let mut b = self.band;
        if b + 1 < BANDS.len() && snr_3k > BANDS[b + 1].0 + BAND_HYST_DB {
            b += 1;
        } else if b > 0 && snr_3k < BANDS[b].0 - BAND_HYST_DB {
            b -= 1;
        }
        if b != self.band {
            // The noise part of the slow average scales with the band, so it
            // is carried over rather than relearned.
            let k = BANDS[b].1 / BANDS[self.band].1;
            if let Some(slow) = &mut self.slow {
                let sig = slow.0.max(0.0);
                slow.1 = sig + (slow.1 - sig) * k;
            }
            let sig = self.sig_s.max(0.0);
            self.noise_s = sig + (self.noise_s - sig) * k;
            self.band = b;
            self.fir.set_cutoff(BANDS[b].1, self.fs);
        }
    }

    /// A tone that holds most of the frequency variance's share at its bin
    /// over the last two seconds, block after block, and where it began.
    fn persisted(&self) -> Option<(AptTone, u64)> {
        if self.ring.len() < TONE_RING {
            return None;
        }
        let var: f64 = self.ring.iter().map(|b| b.var).sum();
        if var <= 0.0 {
            return None;
        }
        let mut best: Option<(usize, f64)> = None;
        for k in 0..3 {
            let share = self.ring.iter().map(|b| b.power[k]).sum::<f64>() / var;
            let hits = self.ring.iter().filter(|b| b.share(k) > BLOCK_SHARE).count();
            if share > TONE_SHARE && hits >= TONE_HITS && best.is_none_or(|(_, s)| share > s) {
                best = Some((k, share));
            }
        }
        let (k, _) = best?;
        // The first block may be partly picture; half a block later is
        // closer to where the tone really began.
        let onset = self.ring.iter().find(|b| b.share(k) > BLOCK_SHARE)?.start + self.blk_len as u64 / 2;
        Some((TONES[k], onset))
    }

    fn set_tone_center(&mut self) {
        if self.off_n > 0 {
            let c = (self.off_sum / self.off_n as f64).clamp(-200.0, 200.0) as f32;
            self.levels = Levels::centered(c);
            self.level_source = LevelSource::StartTone;
        }
    }

    fn start_on(&mut self, tone: AptTone) {
        let ioc = if tone == AptTone::Start288 { 288 } else { 576 };
        if ioc != self.ioc {
            self.ioc = ioc;
            self.reconfigure_clock();
        }
        // The alternation is symmetric about the carrier, so its mean
        // frequency is the tuning error.
        let k = TONES.iter().position(|&t| t == tone).unwrap_or(0);
        let (sum, n) = self
            .ring
            .iter()
            .filter(|b| b.share(k) > BLOCK_SHARE)
            .fold((0.0, 0u32), |(s, n), b| (s + b.mean_hz, n + 1));
        self.off_sum = sum;
        self.off_n = n;
        self.levels = Levels::centered(0.0);
        self.level_source = LevelSource::Nominal;
        self.set_tone_center();
        self.opening = Some(tone);
        self.tone_misses = 0;
        self.ring.clear();
        self.begin(Mode::Starting);
    }

    fn begin(&mut self, mode: Mode) {
        self.mode = mode;
        self.emitted = 0;
        self.shift = None;
        self.pulse = None;
        self.prev_raw = None;
        self.suppress = false;
        self.held.clear();
        self.phase = PhaseSearch::default();
        self.loss.clear();
        self.loss_sum = (0.0, 0.0);
        self.auto_slant_ppm = 0.0;
        let (w, spl) = (self.width(), self.samples_per_line());
        self.clock.width = w;
        self.clock.set_spl(spl);
        self.events.push(WefaxEvent::Start { ioc: self.ioc, lpm: self.lpm });
    }

    /// End the image. Held rows that ended after `cut` (the onset of a stop
    /// tone, a new start tone, or a carrier loss) are dropped; the rest are
    /// released first.
    fn finish(&mut self, cut: Option<u64>) {
        self.level_from_picture();
        while let Some((end, line)) = self.held.pop_front() {
            if cut.is_none_or(|c| end <= c) {
                self.events.push(WefaxEvent::Line(self.levels.pixels(&line)));
                self.emitted += 1;
            }
        }
        self.events.push(WefaxEvent::Stop { lines: self.emitted });
        self.mode = Mode::Idle;
        self.phase = PhaseSearch::default();
        self.prev_raw = None;
        self.opening = None;
        self.ring.clear();
        self.loss.clear();
        self.loss_sum = (0.0, 0.0);
    }

    fn on_raw_line(&mut self, raw: Vec<f32>) {
        match self.mode {
            Mode::Phasing => self.on_phasing_line(raw),
            Mode::Receiving => {
                if let Some(row) = self.assemble(raw) {
                    self.emit(row);
                }
            }
            _ => {}
        }
    }

    fn on_phasing_line(&mut self, raw: Vec<f32>) {
        let w = raw.len();
        let tol = tolerance(w);
        let lines_per_s = self.lpm as f64 / 60.0;
        let unit = self.levels.units(&raw);
        let ps = &mut self.phase;
        ps.lines += 1;
        let line_no = ps.lines as f64;
        let mut done = false;
        let mut give_up = false;
        match ps.lock {
            Some((kind, pos)) => {
                let f = fit(&unit, kind, Some((pos.round() as usize % w, tol)));
                if f.confirms(kind) {
                    ps.run_miss = 0;
                    // Unwrap so the track stays continuous across the edge.
                    let mut p = f.pos;
                    while p - pos > w as f64 / 2.0 {
                        p -= w as f64;
                    }
                    while pos - p > w as f64 / 2.0 {
                        p += w as f64;
                    }
                    ps.lock = Some((kind, p));
                    ps.count += 1;
                    ps.track.push((line_no, p));
                    ps.misses.clear();
                    if ps.recent.len() == PHASING_AVG {
                        ps.recent.pop_front();
                    }
                    if ps.count < PHASING_MIN {
                        ps.early.push(raw.clone());
                    } else {
                        ps.early.clear();
                    }
                    ps.recent.push_back(raw);
                    done = ps.count as f64 >= PHASING_CAP_S * lines_per_s;
                } else if ps.count >= PHASING_MIN {
                    ps.misses.push(raw);
                    done = ps.misses.len() >= PHASING_MISSES;
                } else {
                    ps.run_miss += 1;
                    ps.early.push(raw.clone());
                    if ps.run_miss >= LOCK_MISSES {
                        *ps = PhaseSearch { lines: ps.lines, early: std::mem::take(&mut ps.early), ..Default::default() };
                    }
                    if ps.acq.len() == PHASING_ACQ {
                        ps.acq.pop_front();
                    }
                    ps.acq.push_back(unit);
                }
            }
            None => {
                ps.early.push(raw.clone());
                if ps.acq.len() == PHASING_ACQ {
                    ps.acq.pop_front();
                }
                ps.acq.push_back(unit);
                let k = ps.acq.len();
                let mut avg = vec![0f32; w];
                for l in ps.acq.iter().filter(|l| l.len() == w) {
                    for (a, &v) in avg.iter_mut().zip(l) {
                        *a += v / k as f32;
                    }
                }
                let best = Pulse::ALL
                    .iter()
                    .map(|&kind| (kind, fit(&avg, kind, None)))
                    .filter(|(kind, f)| f.acquires(*kind))
                    .max_by(|a, b| a.1.contrast(a.0).total_cmp(&b.1.contrast(b.0)));
                if let Some((kind, f)) = best {
                    // The averaged lines that show the pulse there on their
                    // own are phasing lines already.
                    let start = ps.early.len().saturating_sub(k);
                    let mut count = 0;
                    for (i, l) in ps.acq.iter().enumerate() {
                        let g = fit(l, kind, Some((f.pos.round() as usize % w, tol)));
                        if g.confirms(kind) {
                            count += 1;
                            ps.track.push((line_no - (k - 1 - i) as f64, f.pos));
                            if let Some(l) = ps.early.get(start + i) {
                                ps.recent.push_back(l.clone());
                            }
                        }
                    }
                    if count > 0 {
                        ps.lock = Some((kind, f.pos));
                        ps.count = count;
                    } else {
                        ps.track.clear();
                        ps.recent.clear();
                    }
                }
            }
        }
        if ps.lock.is_none() || ps.count < PHASING_MIN {
            give_up = ps.lines as f64 > PHASING_GIVEUP_S * lines_per_s;
        }
        if done {
            self.phased();
        } else if give_up {
            self.unphased();
        }
    }

    /// Phasing is over: average the last phasing lines, find the pulse's
    /// leading edge, measure black and white on them, and estimate the
    /// clock error from how the pulse moved; then carry on from the lines
    /// already past it.
    fn phased(&mut self) {
        let ps = std::mem::take(&mut self.phase);
        let (Some((pulse, _)), Some(last)) = (ps.lock, ps.recent.back()) else {
            self.phase = ps;
            self.unphased();
            return;
        };
        let w = last.len();
        let lines: Vec<&Vec<f32>> = ps.recent.iter().filter(|l| l.len() == w).collect();
        let k = lines.len().max(1) as f32;
        let mut avg_hz = vec![0f32; w];
        for l in &lines {
            for (a, &v) in avg_hz.iter_mut().zip(l.iter()) {
                *a += v / k;
            }
        }
        let mut fitted: Option<f64> = None;
        // Slant: a straight line through the pulse positions. The pulse
        // moving `b` pixels a line means the true line is `b / w` longer.
        if ps.track.len() >= SLANT_MIN_LINES {
            let n = ps.track.len() as f64;
            let (mx, my) = ps.track.iter().fold((0.0, 0.0), |(a, b), &(x, y)| (a + x / n, b + y / n));
            let sxx: f64 = ps.track.iter().map(|&(x, _)| (x - mx).powi(2)).sum();
            let sxy: f64 = ps.track.iter().map(|&(x, y)| (x - mx) * (y - my)).sum();
            if sxx > 0.0 {
                let slope = sxy / sxx;
                let resid: f64 = ps.track.iter().map(|&(x, y)| (y - my - slope * (x - mx)).powi(2)).sum();
                let se = (resid / (n - 2.0).max(1.0) / sxx).sqrt();
                let ppm = slope / w as f64 * 1e6;
                // Only a clear, plausible slope: a wrong one would lean the
                // whole chart.
                if slope.abs() > 4.0 * se.max(0.002) && ppm.abs() <= SLANT_MAX_PPM {
                    self.auto_slant_ppm = ppm;
                    let spl = self.samples_per_line();
                    self.clock.set_spl(spl);
                    // Where the drift has carried the pulse by now; the
                    // clock holds it there from here on.
                    fitted = Some(my + slope * (ps.lines as f64 - mx));
                }
            }
        }

        // The line start: the median of where the recent pulses began, or
        // the slant fit's. A median, because with two paths the edge comes
        // from whichever is stronger at the time, and the positions split
        // into two groups a delay apart; an average would sit between them
        // and fit neither.
        let pos_f = fitted.unwrap_or_else(|| {
            let mut recent: Vec<f32> = ps.track.iter().rev().take(PHASING_AVG).map(|&(_, p)| p as f32).collect();
            median(&mut recent) as f64
        });
        let pos = (pos_f.round() as i64).rem_euclid(w as i64) as usize;

        // Levels: medians well inside the pulse and well away from it, on
        // the averaged line. The averaging and the medians keep noise from
        // pulling them in.
        let d = pulse.len(w);
        let (mut inside, mut rest) = (Vec::new(), Vec::new());
        for (i, &v) in avg_hz.iter().enumerate() {
            let off = (i + w - pos) % w;
            if off >= d / 5 && off + d / 5 < d {
                inside.push(v);
            } else if off >= d + d / 2 && off + d / 2 < w {
                rest.push(v);
            }
        }
        let (a, b) = (median(&mut inside), median(&mut rest));
        let (white, black) = if pulse.white() { (a, b) } else { (b, a) };
        let span = white - black;
        if (550.0..=1100.0).contains(&span) && ((white + black) / 2.0).abs() <= 250.0 {
            self.levels = Levels { black_hz: black, white_hz: white };
            self.level_source = LevelSource::Phasing;
        }

        self.shift = Some(pos);
        self.pulse = Some(pulse);
        self.events.push(WefaxEvent::Phased { shift_px: pos as i32 });
        self.mode = Mode::Receiving;
        self.prev_raw = ps.recent.back().cloned();
        self.suppress = true;
        for raw in ps.misses {
            self.on_raw_line(raw);
        }
    }

    /// No phasing was found: the image starts unaligned with the lines
    /// heard since the start tone.
    fn unphased(&mut self) {
        let ps = std::mem::take(&mut self.phase);
        self.shift = Some(0);
        self.mode = Mode::Receiving;
        self.suppress = false;
        for raw in ps.early {
            self.on_raw_line(raw);
        }
    }

    /// One image row from the raw lines: the tail of the previous raw line
    /// from the line start on, then the head of this one up to it. Rotating
    /// within one raw line would join the end of the row before to the start
    /// of this one.
    fn assemble(&mut self, raw: Vec<f32>) -> Option<Vec<f32>> {
        let p = self.shift.unwrap_or(0);
        if p == 0 || p >= raw.len() {
            self.prev_raw = None;
            return Some(raw);
        }
        let row = self.prev_raw.take().filter(|prev| prev.len() == raw.len()).map(|prev| {
            let mut row = Vec::with_capacity(raw.len());
            row.extend_from_slice(&prev[p..]);
            row.extend_from_slice(&raw[..p]);
            row
        });
        self.prev_raw = Some(raw);
        row
    }

    fn emit(&mut self, row: Vec<f32>) {
        if self.suppress {
            // The first aligned rows can still be phasing lines: phasing
            // ends a line or two after the last one is seen.
            if let Some(pulse) = self.pulse {
                let w = row.len();
                let f = fit(&self.levels.units(&row), pulse, Some((0, tolerance(w))));
                let rest_ok = if pulse.white() { f.rest <= ACQ_REST } else { f.rest >= 1.0 - ACQ_REST };
                if f.confirms(pulse) && rest_ok {
                    return;
                }
            }
            self.suppress = false;
        }
        // A rotated row ended at the line start within the raw line that
        // just finished, not at that raw line's end.
        let w = row.len();
        let p = self.shift.unwrap_or(0) % w.max(1);
        let late = if p == 0 { 0 } else { ((w - p) as f64 / w as f64 * self.clock.spl) as u64 };
        self.held.push_back((self.n.saturating_sub(late), row));
        let hold = (HOLD_S * self.fs_dec) as u64;
        while self.held.front().is_some_and(|(end, _)| end + hold < self.n) {
            if self.emitted == 0 {
                self.level_from_picture();
            }
            let (_, line) = self.held.pop_front().expect("front checked");
            self.events.push(WefaxEvent::Line(self.levels.pixels(&line)));
            self.emitted += 1;
            if self.emitted >= MAX_LINES {
                self.finish(None);
                return;
            }
        }
    }

    /// With nothing measured (an image begun by `start_now`), level white
    /// to the picture's dominant light tone, which on a chart is the paper:
    /// a station a little off its nominal frequency otherwise draws the
    /// paper gray. Done once, on the rows held before the first is
    /// released, and only when one light tone clearly dominates, so a
    /// photograph is left alone.
    fn level_from_picture(&mut self) {
        if self.level_source != LevelSource::Nominal || self.emitted > 0 || self.held.len() < 3 {
            return;
        }
        const BIN_HZ: f32 = 10.0;
        const LO: f32 = -800.0;
        let mut hist = [0u32; 160];
        let mut total = 0u32;
        for (_, row) in &self.held {
            for &f in row {
                let b = ((f - LO) / BIN_HZ) as isize;
                if (0..160).contains(&b) {
                    hist[b as usize] += 1;
                }
                total += 1;
            }
        }
        // Only light tones: above the nominal center by a quarter of the
        // deviation.
        let first = ((DEVIATION_HZ / 4.0 - LO) / BIN_HZ) as usize;
        // Mass within ±60 Hz: wide enough to hold most of a noisy peak.
        let mass = |b: usize| hist[b.saturating_sub(6)..(b + 7).min(160)].iter().sum::<u32>();
        let Some(peak) = (first..160).max_by_key(|&b| mass(b)) else { return };
        if (mass(peak) as f32) < 0.2 * total as f32 {
            return;
        }
        // The mean over ±100 Hz about the peak places it: symmetric, so
        // noise about the paper's tone does not move it.
        let (mut s, mut m) = (0f32, 0f32);
        let lo = peak.saturating_sub(10);
        for (b, &h) in hist.iter().enumerate().take((peak + 11).min(160)).skip(lo) {
            let c = LO + (b as f32 + 0.5) * BIN_HZ;
            s += c * h as f32;
            m += h as f32;
        }
        let white = s / m.max(1.0);
        if (white - DEVIATION_HZ).abs() > 15.0 && (white - DEVIATION_HZ).abs() <= 250.0 {
            self.levels = Levels { black_hz: white - 2.0 * DEVIATION_HZ, white_hz: white };
            self.level_source = LevelSource::Picture;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use num_complex::Complex64;

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn uniform(&mut self) -> f64 {
            (self.next() >> 11) as f64 / (1u64 << 53) as f64
        }
        fn gauss(&mut self) -> f64 {
            let u1 = self.uniform().max(1e-300);
            let u2 = self.uniform();
            (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
        }
    }

    /// The source picture: a horizontal gradient, black vertical bars, and a
    /// white diagonal line.
    fn image(row: usize, col: usize, w: usize) -> u8 {
        let diag = 300 * w / 1809 + row * 20 * w / 1809;
        if col.abs_diff(diag) < 3 * w / 1809 + 1 {
            255
        } else if (col * 1809 / w / 150) % 4 == 3 {
            0
        } else {
            (col * 255 / w) as u8
        }
    }

    /// A two-path HF channel after Watterson (ITU-R F.1487): each path is
    /// Rayleigh-faded by complex Gaussian noise with a Gaussian Doppler
    /// spectrum whose two-sigma width is `spread_hz`, the second arriving
    /// `delay_s` later. Both paths carry half the power, so the average SNR
    /// is the one asked for.
    struct Fading {
        /// Path gains at `RATE` Hz.
        g: [Vec<Complex64>; 2],
        delay: usize,
    }

    impl Fading {
        const RATE: f64 = 20.0;

        fn new(rng: &mut Rng, spread_hz: f64, delay_s: f64, fs: f64, secs: f64) -> Self {
            let sigma_k = Self::RATE / (std::f64::consts::TAU * spread_hz / 2.0);
            let half = (4.0 * sigma_k).ceil() as i64;
            let mut h: Vec<f64> = (-half..=half).map(|k| (-(k * k) as f64 / (2.0 * sigma_k * sigma_k)).exp()).collect();
            let norm = h.iter().map(|x| x * x).sum::<f64>().sqrt();
            h.iter_mut().for_each(|x| *x /= norm);
            let n = (secs * Self::RATE) as usize + 2;
            let mut path = || {
                let w: Vec<Complex64> = (0..n + h.len())
                    .map(|_| Complex64::new(rng.gauss(), rng.gauss()) * 0.5f64.sqrt())
                    .collect();
                (0..n)
                    .map(|i| h.iter().zip(&w[i..]).map(|(a, b)| b * *a).sum::<Complex64>() * 0.5f64.sqrt())
                    .collect::<Vec<_>>()
            };
            let g = [path(), path()];
            Self { g, delay: (delay_s * fs).round() as usize }
        }

        /// The gains at `t`, the table repeating past its end.
        fn at(&self, t: f64) -> (Complex64, Complex64) {
            let n = self.g[0].len();
            let x = (t * Self::RATE) % (n - 1) as f64;
            let i = (x as usize).min(n - 2);
            let f = x - i as f64;
            let lerp = |g: &Vec<Complex64>| g[i] * (1.0 - f) + g[i + 1] * f;
            (lerp(&self.g[0]), lerp(&self.g[1]))
        }
    }

    /// A fax transmitter, an HF channel and a sideband receiver's audio
    /// output, at a sample clock `ppm` fast, with white noise for `snr_db` in
    /// 3 kHz and, optionally, fading and impulse noise.
    struct Synth {
        fs: f64,
        carrier: f64,
        amp: f64,
        sigma: f64,
        phase: f64,
        rng: Rng,
        out: Vec<f32>,
        /// Real time of the next sample.
        t: f64,
        fading: Option<Fading>,
        /// The transmitted phasor's recent past, for the delayed path.
        hist: Vec<Complex64>,
        hist_at: usize,
        /// Bursts per second and their level against the signal's amplitude.
        impulses: Option<(f64, f64)>,
        burst_left: usize,
    }

    impl Synth {
        fn new(seed: u64, amp: f64, snr_db: f64, carrier: f64, ppm: f64) -> Self {
            // Signal power amp²/2 against noise σ²·3000/(fs/2) in 3 kHz.
            let snr = 10f64.powf(snr_db / 10.0);
            let sigma = 2.0 * amp / snr.sqrt();
            // splitmix64, so small seeds give unrelated streams.
            let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            Self {
                fs: 48_000.0 * (1.0 + ppm * 1e-6),
                carrier,
                amp,
                sigma,
                phase: 0.0,
                rng: Rng(z | 1),
                out: Vec::new(),
                t: 0.0,
                fading: None,
                hist: Vec::new(),
                hist_at: 0,
                impulses: None,
                burst_left: 0,
            }
        }

        fn with_fading(mut self, spread_hz: f64, delay_s: f64) -> Self {
            let fd = Fading::new(&mut self.rng, spread_hz, delay_s, self.fs, 400.0);
            self.hist = vec![Complex64::new(0.0, 0.0); fd.delay.max(1)];
            self.fading = Some(fd);
            self
        }

        fn with_impulses(mut self, per_s: f64, rel: f64) -> Self {
            self.impulses = Some((per_s, rel));
            self
        }

        /// `level(t)` gives the pixel (0..255) at `t` seconds into the
        /// section, or `None` for no signal.
        fn section(&mut self, secs: f64, level: impl Fn(f64) -> Option<u8>) {
            let t0 = self.t;
            while self.t < t0 + secs {
                let lv = level(self.t - t0);
                let mut p = Complex64::new(0.0, 0.0);
                if let Some(v) = lv {
                    let f = self.carrier - 400.0 + 800.0 * v as f64 / 255.0;
                    self.phase = (self.phase + std::f64::consts::TAU * f / self.fs) % std::f64::consts::TAU;
                    p = Complex64::from_polar(1.0, self.phase);
                }
                let y = match &self.fading {
                    None => p,
                    Some(fd) => {
                        let late = std::mem::replace(&mut self.hist[self.hist_at], p);
                        self.hist_at = (self.hist_at + 1) % self.hist.len();
                        let (g1, g2) = fd.at(self.t);
                        g1 * p + g2 * late
                    }
                };
                let mut s = self.amp * y.re;
                s += self.sigma * self.rng.gauss();
                if let Some((rate, rel)) = self.impulses {
                    if self.burst_left > 0 {
                        self.burst_left -= 1;
                        s += rel * self.amp * self.rng.gauss();
                    } else if self.rng.uniform() < rate / self.fs {
                        self.burst_left = ((0.5e-3 + 2.5e-3 * self.rng.uniform()) * self.fs) as usize;
                    }
                }
                self.out.push(s as f32);
                self.t += 1.0 / self.fs;
            }
        }

        fn noise(&mut self, secs: f64) {
            self.section(secs, |_| None);
        }

        fn tone(&mut self, secs: f64, hz: f64) {
            self.section(secs, |t| Some(if (t * hz).fract() < 0.5 { 0 } else { 255 }));
        }

        fn lines(&mut self, n: usize, lpm: f64, w: usize, px: impl Fn(usize, usize) -> u8) {
            let period = 60.0 / lpm;
            self.section(n as f64 * period, |t| {
                let line = (t / period) as usize;
                let col = (((t / period).fract()) * w as f64) as usize;
                Some(px(line.min(n - 1), col.min(w - 1)))
            });
        }

        fn phasing(&mut self, n: usize, lpm: f64, w: usize) {
            let d = (w as f64 * 0.05).round() as usize;
            self.lines(n, lpm, w, |_, c| if c < d { 255 } else { 0 });
        }

        /// The whole transmission: noise lead of random length (so the image
        /// starts at a random phase of the decoder's line clock), start tone,
        /// phasing, the picture, stop tone, black.
        fn fax(&mut self, start_hz: f64, rows: usize, w: usize) {
            let lead = 1.0 + self.rng.uniform();
            self.noise(lead);
            self.tone(5.0, start_hz);
            self.phasing(20, 120.0, w);
            self.lines(rows, 120.0, w, |r, c| image(r, c, w));
            self.tone(5.0, 450.0);
            self.section(5.0, |_| Some(0));
            self.noise(2.0);
        }
    }

    fn run(dec: &mut WefaxDecoder, audio: &[f32], seed: u64) -> Vec<WefaxEvent> {
        let mut rng = Rng(seed | 1);
        let mut events = Vec::new();
        let mut i = 0;
        while i < audio.len() {
            let n = (300 + (rng.next() % 2700) as usize).min(audio.len() - i);
            dec.push(&audio[i..i + n]);
            events.extend(dec.take_events());
            i += n;
        }
        events
    }

    fn rows(events: &[WefaxEvent]) -> Vec<&Vec<u8>> {
        events.iter().filter_map(|e| if let WefaxEvent::Line(l) = e { Some(l) } else { None }).collect()
    }

    /// Best (mean abs error, row offset, column shift) of the decoded rows
    /// against the source, searching a few rows and `cols` columns.
    fn align(got: &[&Vec<u8>], src_rows: usize, w: usize, cols: i32) -> (f64, i32, i32) {
        let mut best = (f64::MAX, 0, 0);
        for dr in -2i32..=2 {
            for dc in -cols..=cols {
                let (mut err, mut n) = (0u64, 0u64);
                for (r, row) in got.iter().enumerate() {
                    let sr = r as i32 + dr;
                    if sr < 0 || sr as usize >= src_rows {
                        continue;
                    }
                    for (c, &v) in row.iter().enumerate() {
                        let sc = (c as i32 + dc).rem_euclid(w as i32) as usize;
                        err += v.abs_diff(image(sr as usize, sc, w)) as u64;
                        n += 1;
                    }
                }
                if n > (w * src_rows / 2) as u64 {
                    let mae = err as f64 / n as f64;
                    if mae < best.0 {
                        best = (mae, dr, dc);
                    }
                }
            }
        }
        best
    }

    fn kinds(events: &[WefaxEvent]) -> String {
        let mut s = String::new();
        for e in events {
            let c = match e {
                WefaxEvent::Start { .. } => 'S',
                WefaxEvent::Phased { .. } => 'P',
                WefaxEvent::Line(_) => 'L',
                WefaxEvent::Stop { .. } => 'E',
            };
            if c != 'L' || !s.ends_with('L') {
                s.push(c);
            }
        }
        s
    }

    fn check_fax(seed: u64, amp: f64, snr_db: f64, carrier: f64, ppm: f64, max_mae: f64) -> f64 {
        const ROWS: usize = 60;
        let mut syn = Synth::new(seed, amp, snr_db, carrier, ppm);
        syn.fax(300.0, ROWS, 1809);
        let mut dec = WefaxDecoder::new(48_000.0);
        dec.set_slant_ppm(ppm);
        let events = run(&mut dec, &syn.out, seed ^ 0x55);
        assert_eq!(kinds(&events), "SPLE", "events {:?}", kinds(&events));
        assert!(matches!(events[0], WefaxEvent::Start { ioc: 576, .. }));
        let got = rows(&events);
        assert!(got.iter().all(|r| r.len() == 1809));
        assert!(got.len().abs_diff(ROWS) <= 2, "{} rows", got.len());
        let Some(WefaxEvent::Stop { lines }) = events.last() else { panic!() };
        assert_eq!(*lines as usize, got.len());
        let (mae, dr, dc) = align(&got, ROWS, 1809, 8);
        let shift = events.iter().find_map(|e| if let WefaxEvent::Phased { shift_px } = e { Some(*shift_px) } else { None });
        println!(
            "seed {seed} snr {snr_db} dB carrier {carrier} ppm {ppm}: {} rows, shift {shift:?}, mae {mae:.2}, row offset {dr}, col offset {dc}, snr est {:?}, offset est {:.1} Hz",
            got.len(),
            dec.carrier_snr_db(),
            dec.carrier_offset_hz()
        );
        assert!(dc.abs() <= 3, "column offset {dc}");
        assert!(dr.abs() <= 1, "row offset {dr}");
        assert!(mae < max_mae, "mae {mae}");
        assert_eq!(dec.state(), WefaxState::Idle);
        mae
    }

    #[test]
    fn wefax_full_transmission() {
        for seed in [1u64, 7, 12345, 999] {
            check_fax(seed, 3e-4, 30.0, 1900.0, 0.0, 12.0);
        }
    }

    #[test]
    fn wefax_poor_snr() {
        // 10 dB in 3 kHz: noisy but recognizable. Correlation with the
        // source is checked as well as the error.
        const ROWS: usize = 60;
        let mut syn = Synth::new(42, 2.5, 10.0, 1900.0, 0.0);
        syn.fax(300.0, ROWS, 1809);
        let mut dec = WefaxDecoder::new(48_000.0);
        let events = run(&mut dec, &syn.out, 3);
        assert_eq!(kinds(&events), "SPLE");
        let got = rows(&events);
        assert!(got.len().abs_diff(ROWS) <= 2);
        let (mae, dr, dc) = align(&got, ROWS, 1809, 8);
        let (mut sx, mut sy, mut sxx, mut syy, mut sxy, mut n) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        for (r, row) in got.iter().enumerate() {
            let sr = r as i32 + dr;
            if sr < 0 || sr as usize >= ROWS {
                continue;
            }
            for (c, &v) in row.iter().enumerate() {
                let x = image(sr as usize, (c as i32 + dc).rem_euclid(1809) as usize, 1809) as f64;
                let y = v as f64;
                sx += x;
                sy += y;
                sxx += x * x;
                syy += y * y;
                sxy += x * y;
                n += 1.0;
            }
        }
        let corr = (sxy / n - sx / n * sy / n) / ((sxx / n - (sx / n).powi(2)).sqrt() * (syy / n - (sy / n).powi(2)).sqrt());
        println!("10 dB: mae {mae:.2}, corr {corr:.3}, offset {dr}/{dc}, snr est {:?}", dec.carrier_snr_db());
        assert!(dc.abs() <= 3);
        assert!(mae < 30.0, "mae {mae}");
        assert!(corr > 0.9, "corr {corr}");
    }

    #[test]
    fn wefax_carrier_offset_and_clock_error() {
        check_fax(77, 0.02, 30.0, 1920.0, 50.0, 12.0);
        check_fax(78, 0.02, 30.0, 1880.0, -50.0, 12.0);
    }

    #[test]
    fn wefax_start_now_mid_image() {
        const ROWS: usize = 60;
        let mut syn = Synth::new(5, 1.0, 30.0, 1900.0, 0.0);
        syn.lines(ROWS, 120.0, 1809, |r, c| image(r, c, 1809));
        let mut dec = WefaxDecoder::new(48_000.0);
        let split = 48_000 * 3 + 12_345;
        let mut events = run(&mut dec, &syn.out[..split], 9);
        assert!(events.is_empty(), "{events:?}");
        assert_eq!(dec.state(), WefaxState::Idle);
        dec.start_now();
        events.extend(run(&mut dec, &syn.out[split..], 10));
        assert!(matches!(dec.state(), WefaxState::Receiving { .. }));
        dec.stop_now();
        events.extend(dec.take_events());
        assert_eq!(kinds(&events), "SLE");
        let got = rows(&events);
        // 30 s of picture from 3.26 s in: about 53 rows, the last few of
        // them still held when stop_now released them.
        assert!(got.len() >= 50, "{} rows", got.len());
        // Unphased, so the rows start anywhere: search every offset in time,
        // counted in pixels from the start of the source picture.
        let mut best = f64::MAX;
        for off in 5 * 1809..8 * 1809 {
            let (mut err, mut n) = (0u64, 0u64);
            for (r, row) in got.iter().enumerate().take(40).skip(2) {
                for (c, &v) in row.iter().enumerate().step_by(7) {
                    let t = r * 1809 + c + off;
                    let (sr, sc) = (t / 1809, t % 1809);
                    if sr < ROWS {
                        err += v.abs_diff(image(sr, sc, 1809)) as u64;
                        n += 1;
                    }
                }
            }
            if n > 0 {
                best = best.min(err as f64 / n as f64);
            }
        }
        println!("start_now: {} rows, mae {best:.2}", got.len());
        assert!(best < 12.0, "mae {best}");
    }

    #[test]
    fn wefax_ioc_288_switches_width() {
        let mut syn = Synth::new(11, 0.1, 30.0, 1900.0, 0.0);
        syn.fax(675.0, 40, 904);
        let mut dec = WefaxDecoder::new(48_000.0);
        assert_eq!(dec.width(), 1809);
        let events = run(&mut dec, &syn.out, 12);
        assert_eq!(kinds(&events), "SPLE");
        assert!(matches!(events[0], WefaxEvent::Start { ioc: 288, .. }));
        assert_eq!(dec.width(), 904);
        assert_eq!(dec.ioc(), 288);
        let got = rows(&events);
        assert!(got.iter().all(|r| r.len() == 904));
        assert!(got.len().abs_diff(40) <= 2);
        let (mae, _, dc) = align(&got, 40, 904, 6);
        println!("IOC 288: {} rows, mae {mae:.2}, col offset {dc}", got.len());
        assert!(mae < 12.0 && dc.abs() <= 2);
    }

    #[test]
    fn wefax_noise_never_starts() {
        for (seed, amp) in [(21u64, 1e-4), (22, 1.0)] {
            let mut syn = Synth::new(seed, amp, -100.0, 1900.0, 0.0);
            syn.sigma = amp;
            syn.noise(60.0);
            let mut dec = WefaxDecoder::new(48_000.0);
            let events = run(&mut dec, &syn.out, seed);
            assert!(events.is_empty(), "{events:?}");
            println!("noise: snr est {:?}", dec.carrier_snr_db());
            assert!(dec.carrier_snr_db().is_some_and(|s| s < 3.0));
        }
    }

    /// How much of a fine pattern survives: stripes `period` pixels apart,
    /// measured as the decoded amplitude at that period against the source's.
    fn stripe_response(period: usize) -> f64 {
        let w = 1809;
        let px = move |_: usize, c: usize| if (c % period) < period / 2 { 0u8 } else { 255 };
        let mut syn = Synth::new(period as u64, 0.1, 40.0, 1900.0, 0.0);
        syn.noise(1.0);
        syn.tone(5.0, 300.0);
        syn.phasing(20, 120.0, w);
        syn.lines(20, 120.0, w, px);
        syn.tone(5.0, 450.0);
        let mut dec = WefaxDecoder::new(48_000.0);
        let events = run(&mut dec, &syn.out, 1);
        let got = rows(&events);
        let amp = |row: &[u8]| {
            let (mut re, mut im) = (0.0, 0.0);
            for (c, &v) in row.iter().enumerate().take(1800).skip(200) {
                let a = std::f64::consts::TAU * c as f64 / period as f64;
                re += v as f64 * a.cos();
                im += v as f64 * a.sin();
            }
            (re * re + im * im).sqrt()
        };
        let src: Vec<u8> = (0..w).map(|c| px(0, c)).collect();
        let mid = &got[2..got.len() - 2];
        mid.iter().map(|r| amp(r)).sum::<f64>() / mid.len() as f64 / amp(&src)
    }

    /// Stripes 8 pixels apart are left out: at 120 LPM they are a 452 Hz
    /// alternation, which is the stop tone, and two seconds of it stop the
    /// image as the specification intends.
    #[test]
    fn wefax_fine_detail() {
        for period in [3usize, 4, 6, 10] {
            let r = stripe_response(period);
            println!("stripes every {period} px: {:.0} % of their contrast", 100.0 * r);
            if period >= 4 {
                assert!(r > 0.4, "period {period}: {r}");
            }
        }
    }

    #[test]
    fn wefax_snr_reading() {
        for snr in [30.0, 20.0, 10.0, 3.0] {
            let mut syn = Synth::new(snr as u64, 0.05, snr, 1900.0, 0.0);
            syn.lines(30, 120.0, 1809, |r, c| image(r, c, 1809));
            let mut dec = WefaxDecoder::new(48_000.0);
            let mut est = Vec::new();
            for chunk in syn.out.chunks(4800) {
                dec.push(chunk);
                est.extend(dec.carrier_snr_db());
            }
            let tail = &est[est.len() / 2..];
            let mean = tail.iter().sum::<f32>() / tail.len() as f32;
            println!("snr {snr} dB in 3 kHz: estimate {mean:.1} dB");
            // The passband is a little narrower than 3 kHz, and filtering the
            // FM sidebands ripples the envelope, which reads as noise at a
            // high SNR.
            assert!(mean > snr as f32 - 6.0, "{mean}");
        }
    }

    /// A whole transmission through `chan` at `snr`, decoded; the events
    /// and the source picture's score.
    fn through(chan: Chan, snr: f64, seed: u64, rows: usize, phasing: usize, ppm: f64) -> (Vec<WefaxEvent>, f64, i64, WefaxDecoder) {
        let pic = Pic::synthetic();
        let w = 1809;
        let mut syn = synth_for(chan, snr, seed);
        if ppm != 0.0 {
            syn.fs = 48_000.0 * (1.0 + ppm * 1e-6);
        }
        let lead = 1.0 + syn.rng.uniform();
        syn.noise(lead);
        syn.tone(5.0, 300.0);
        if phasing > 0 {
            syn.phasing(phasing, 120.0, w);
        }
        syn.lines(rows, 120.0, w, |r, c| pic.at(r, c));
        syn.tone(5.0, 450.0);
        syn.section(5.0, |_| Some(0));
        syn.noise(2.0);
        let mut dec = WefaxDecoder::new(48_000.0);
        let ev = run(&mut dec, &syn.out, seed);
        let got = rows_of(&ev);
        let (_, corr, off) = score(&got, rows, w, &pic);
        (ev, corr, off.rem_euclid(w as i64), dec)
    }

    fn rows_of(ev: &[WefaxEvent]) -> Vec<&Vec<u8>> {
        rows(ev)
    }

    #[test]
    fn wefax_weak_and_fading() {
        // Points the weak-signal sweep showed holding; none of them started
        // before (the start tone was missed).
        for (chan, snr, min_corr) in [(Chan::Awgn, 0.0, 0.6), (Chan::Moderate, 5.0, 0.65), (Chan::PoorImpulsive, 5.0, 0.6)] {
            let (ev, corr, dc, _) = through(chan, snr, 41, 60, 20, 0.0);
            println!("{chan:?} {snr} dB: {} corr {corr:.3} col {dc}", kinds(&ev));
            assert_eq!(kinds(&ev), "SPLE", "{chan:?} {snr}");
            assert!(corr > min_corr, "{chan:?} {snr}: corr {corr}");
            assert!(dc <= 11 || dc >= 1809 - 11, "{chan:?} {snr}: col {dc}");
        }
    }

    #[test]
    fn wefax_auto_slant() {
        // A sample clock 100 ppm fast and no trim set: the phasing lines
        // measure it, and the picture comes out straight.
        let (ev, corr, dc, dec) = through(Chan::Awgn, 25.0, 5, 120, 40, 100.0);
        println!("auto slant {:.1} ppm, corr {corr:.3}, col {dc}", dec.auto_slant_ppm());
        assert_eq!(kinds(&ev), "SPLE");
        assert!((dec.auto_slant_ppm() - 100.0).abs() < 15.0, "{}", dec.auto_slant_ppm());
        assert!(corr > 0.97, "corr {corr}");
        // Without a clock error, or with fading moving the edge between two
        // paths, no slant is invented.
        for (chan, seed) in [(Chan::Awgn, 6), (Chan::Poor, 7), (Chan::Poor, 8)] {
            let (_, _, _, dec) = through(chan, 15.0, seed, 20, 40, 0.0);
            println!("{chan:?}: auto slant {:.1} ppm", dec.auto_slant_ppm());
            assert!(dec.auto_slant_ppm().abs() < 15.0, "{chan:?}: {}", dec.auto_slant_ppm());
        }
    }

    #[test]
    fn wefax_inverted_phasing() {
        // A black pulse on white: found at a good SNR, and the levels the
        // right way up.
        let pic = Pic::synthetic();
        let w = 1809;
        let mut syn = synth_for(Chan::Awgn, 20.0, 12);
        syn.noise(1.3);
        syn.tone(5.0, 300.0);
        let d = (w as f64 * 0.05).round() as usize;
        syn.lines(20, 120.0, w, |_, c| if c < d { 0 } else { 255 });
        syn.lines(40, 120.0, w, |r, c| pic.at(r, c));
        syn.tone(5.0, 450.0);
        let mut dec = WefaxDecoder::new(48_000.0);
        let ev = run(&mut dec, &syn.out, 12);
        let got = rows(&ev);
        let (_, corr, off) = score(&got, 40, w, &pic);
        let dc = off.rem_euclid(w as i64);
        println!("inverted: {} corr {corr:.3} col {dc} levels {:?}", kinds(&ev), dec.levels_hz());
        assert_eq!(kinds(&ev), "SPLE");
        assert!(dc <= 3 || dc >= w as i64 - 3);
        assert!(corr > 0.95);
        assert_eq!(dec.level_source(), LevelSource::Phasing);
    }

    #[test]
    fn wefax_station_without_phasing() {
        // Start tone straight into the picture: no phasing is invented, and
        // after the wait the picture is drawn from its first rows, unaligned.
        let (ev, corr, _, _) = through(Chan::Awgn, 20.0, 9, 60, 0, 0.0);
        let got = rows(&ev);
        println!("no phasing: {} {} rows corr {corr:.3}", kinds(&ev), got.len());
        assert_eq!(kinds(&ev), "SLE");
        assert!(got.len() >= 58, "{} rows", got.len());
        assert!(corr > 0.95, "corr {corr}");
    }

    #[test]
    fn wefax_carrier_loss() {
        // A picture that fades out for good ends about 45 s later; one at
        // 0 dB in a poor channel for 100 s does not end at all.
        let pic = Pic::synthetic();
        let mut syn = synth_for(Chan::Awgn, 20.0, 3);
        syn.lines(40, 120.0, 1809, |r, c| pic.at(r, c));
        syn.noise(70.0);
        let mut dec = WefaxDecoder::new(48_000.0);
        dec.start_now();
        let mut stop_at = None;
        for (i, c) in syn.out.chunks(4800).enumerate() {
            dec.push(c);
            if dec.take_events().iter().any(|e| matches!(e, WefaxEvent::Stop { .. })) {
                stop_at = Some(i as f64 * 0.1);
            }
        }
        println!("carrier gone at 20 s, stop at {stop_at:?}");
        assert!(stop_at.is_some_and(|t| (45.0..75.0).contains(&t)), "{stop_at:?}");

        let mut syn = synth_for(Chan::Poor, 0.0, 4);
        syn.lines(200, 120.0, 1809, |r, c| pic.at(r, c));
        let mut dec = WefaxDecoder::new(48_000.0);
        dec.start_now();
        let ev = run(&mut dec, &syn.out, 4);
        assert!(!ev.iter().any(|e| matches!(e, WefaxEvent::Stop { .. })), "stopped early");
    }

    #[test]
    fn wefax_bench() {
        let mut syn = Synth::new(31, 0.3, 20.0, 1900.0, 0.0);
        syn.fax(300.0, 120, 1809);
        let secs = syn.out.len() as f64 / 48_000.0;
        let mut dec = WefaxDecoder::new(48_000.0);
        let t = std::time::Instant::now();
        let mut lines = 0;
        for chunk in syn.out.chunks(960) {
            dec.push(chunk);
            lines += dec.take_events().iter().filter(|e| matches!(e, WefaxEvent::Line(_))).count();
        }
        let el = t.elapsed().as_secs_f64();
        println!(
            "wefax bench: {secs:.1} s of audio in {:.1} ms, {:.0}x realtime ({:.3} % of a core), {lines} rows",
            el * 1e3,
            secs / el,
            100.0 * el / secs
        );
        assert!(lines >= 115);
    }

    // ---- Weak-signal measurements -------------------------------------

    /// A source picture for the sweep: the synthetic test image, or a real
    /// chart from a PGM (`USDR_WEFAX_CHART`), repeated down as needed.
    struct Pic {
        w: usize,
        h: usize,
        px: Vec<u8>,
    }

    impl Pic {
        fn synthetic() -> Self {
            let (w, h) = (1809, 200);
            Self { w, h, px: (0..w * h).map(|i| image(i / w, i % w, w)).collect() }
        }

        fn load() -> Self {
            let Some(path) = std::env::var_os("USDR_WEFAX_CHART") else { return Self::synthetic() };
            let b = std::fs::read(path).expect("chart");
            let nl: Vec<usize> = b.iter().enumerate().filter(|(_, c)| **c == b'\n').map(|(i, _)| i).collect();
            let hdr = String::from_utf8_lossy(&b[..nl[0]]).to_string();
            let f: Vec<usize> = hdr.split_whitespace().skip(1).map(|x| x.parse().unwrap()).collect();
            let (w, h) = (f[0], f[1]);
            Self { w, h, px: b[nl[0] + 1..nl[0] + 1 + w * h].to_vec() }
        }

        fn at(&self, r: usize, c: usize) -> u8 {
            self.px[(r % self.h) * self.w + c % self.w]
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum Chan {
        Awgn,
        Moderate,
        Poor,
        PoorImpulsive,
    }

    fn synth_for(chan: Chan, snr: f64, seed: u64) -> Synth {
        let s = Synth::new(seed, 0.01, snr, 1900.0, 0.0);
        match chan {
            Chan::Awgn => s,
            Chan::Moderate => s.with_fading(0.5, 1e-3),
            Chan::Poor => s.with_fading(1.0, 2e-3),
            Chan::PoorImpulsive => s.with_fading(1.0, 2e-3).with_impulses(2.0, 10.0),
        }
    }

    /// Picture error and correlation against the source, at the best
    /// alignment: decoded pixel (r, c) is taken as source pixel time
    /// `r·w + c + off`, so an unphased or misphased picture is still scored
    /// on its content. Returns (mae, corr, off).
    fn score(got: &[&Vec<u8>], src_rows: usize, w: usize, pic: &Pic) -> (f64, f64, i64) {
        let image = |r: usize, c: usize, _w: usize| pic.at(r, c);
        if got.len() < 4 {
            return (f64::NAN, 0.0, 0);
        }
        let mut prof = vec![0f64; w];
        for row in got {
            for (c, &v) in row.iter().enumerate().take(w) {
                prof[c] += v as f64;
            }
        }
        let src: Vec<f64> = (0..w).map(|c| (0..src_rows).map(|r| image(r, c, w) as f64).sum()).collect();
        let (pm, sm) = (prof.iter().sum::<f64>() / w as f64, src.iter().sum::<f64>() / w as f64);
        let mut best_dc = 0;
        let mut best = f64::MIN;
        for dc in 0..w {
            let x: f64 = (0..w).map(|c| (prof[c] - pm) * (src[(c + dc) % w] - sm)).sum();
            if x > best {
                best = x;
                best_dc = dc as i64;
            }
        }
        let eval = |off: i64| {
            let (mut sx, mut sy, mut sxx, mut syy, mut sxy, mut n, mut err) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
            for (r, row) in got.iter().enumerate() {
                for (c, &v) in row.iter().enumerate().step_by(3) {
                    let t = (r * w + c) as i64 + off;
                    if t < 0 || t >= (src_rows * w) as i64 {
                        continue;
                    }
                    let x = image(t as usize / w, t as usize % w, w) as f64;
                    let y = v as f64;
                    sx += x;
                    sy += y;
                    sxx += x * x;
                    syy += y * y;
                    sxy += x * y;
                    err += (x - y).abs();
                    n += 1.0;
                }
            }
            if n < (got.len() * w / 6) as f64 {
                return (f64::NAN, -1.0);
            }
            let corr = (sxy / n - sx / n * sy / n) / ((sxx / n - (sx / n).powi(2)).sqrt() * (syy / n - (sy / n).powi(2)).sqrt()).max(1e-9);
            (err / n, corr)
        };
        let mut out = (f64::NAN, -1.0, 0);
        for k in -4i64..=4 {
            for d in -2i64..=2 {
                let off = best_dc + d + k * w as i64;
                let (mae, corr) = eval(off);
                if corr > out.1 {
                    out = (mae, corr, off);
                }
            }
        }
        out
    }

    #[derive(Default, Clone, Copy)]
    struct Tally {
        n: u32,
        started: u32,
        phased: u32,
        early: u32,
        false_events: u32,
        corr: f64,
        man_mae: f64,
        man_corr: f64,
    }

    const SWEEP_ROWS: usize = 140;

    fn trial_auto(chan: Chan, snr: f64, seed: u64, t: &mut Tally, pic: &Pic) {
        let w = 1809;
        let mut syn = synth_for(chan, snr, seed);
        let lead = 1.0 + syn.rng.uniform();
        syn.noise(lead);
        syn.tone(5.0, 300.0);
        syn.phasing(20, 120.0, w);
        syn.lines(SWEEP_ROWS, 120.0, w, |r, c| pic.at(r, c));
        syn.tone(5.0, 450.0);
        syn.section(5.0, |_| Some(0));
        syn.noise(2.0);
        let mut dec = WefaxDecoder::new(48_000.0);
        let ev = run(&mut dec, &syn.out, seed);
        let starts = ev.iter().filter(|e| matches!(e, WefaxEvent::Start { .. })).count();
        let stops = ev.iter().filter(|e| matches!(e, WefaxEvent::Stop { .. })).count();
        let got = rows(&ev);
        let (_, corr, off) = score(&got, SWEEP_ROWS, w, pic);
        t.n += 1;
        if starts >= 1 {
            t.started += 1;
        }
        if starts > 1 || stops > 1 {
            t.false_events += 1;
        }
        let phased = ev.iter().any(|e| matches!(e, WefaxEvent::Phased { .. }));
        let dc = off.rem_euclid(w as i64);
        // With two paths the picture is drawn twice, the delay apart, and
        // either copy is a right alignment.
        let tol = match chan {
            Chan::Awgn => 3,
            Chan::Moderate => 3 + 4,
            Chan::Poor | Chan::PoorImpulsive => 3 + 8,
        };
        if phased && (dc <= tol || dc >= w as i64 - tol) {
            t.phased += 1;
        }
        if starts >= 1 && got.len() + 3 < SWEEP_ROWS {
            t.early += 1;
        }
        t.corr += corr.max(0.0);
    }

    fn trial_manual(chan: Chan, snr: f64, seed: u64, t: &mut Tally, pic: &Pic) {
        let w = 1809;
        let rows_n = 40;
        let mut syn = synth_for(chan, snr, seed ^ 0xabc);
        syn.noise(1.0);
        let at = syn.out.len();
        syn.lines(rows_n, 120.0, w, |r, c| pic.at(r, c));
        let mut dec = WefaxDecoder::new(48_000.0);
        let mut ev = run(&mut dec, &syn.out[..at], seed);
        dec.start_now();
        ev.extend(run(&mut dec, &syn.out[at..], seed + 1));
        dec.stop_now();
        ev.extend(dec.take_events());
        let got = rows(&ev);
        let (mae, corr, _) = score(&got, rows_n, w, pic);
        t.man_mae += if mae.is_nan() { 128.0 } else { mae };
        t.man_corr += corr.max(0.0);
    }

    /// The weak-signal table: `USDR_WEFAX_SWEEP=<seeds>` runs it.
    #[test]
    fn wefax_weak_signal_sweep() {
        let Ok(seeds) = std::env::var("USDR_WEFAX_SWEEP") else { return };
        let seeds: u64 = seeds.parse().unwrap_or(4);
        let chans = [Chan::Awgn, Chan::Moderate, Chan::Poor, Chan::PoorImpulsive];
        let snrs = [15.0, 10.0, 5.0, 0.0, -3.0];
        let points: Vec<(Chan, f64)> = chans.iter().flat_map(|&c| snrs.iter().map(move |&s| (c, s))).collect();
        let pic = Pic::load();
        let pic = &pic;
        let results: Vec<(Chan, f64, Tally)> = std::thread::scope(|sc| {
            let hs: Vec<_> = points
                .iter()
                .map(|&(c, s)| {
                    sc.spawn(move || {
                        let mut t = Tally::default();
                        for seed in 0..seeds {
                            trial_auto(c, s, 1000 + seed, &mut t, pic);
                            trial_manual(c, s, 2000 + seed, &mut t, pic);
                        }
                        (c, s, t)
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        println!("| channel | SNR 3k | start | phased | early end | extra events | auto corr | manual MAE | manual corr |");
        println!("|---|---|---|---|---|---|---|---|---|");
        for (c, s, t) in results {
            let n = t.n as f64;
            println!(
                "| {c:?} | {s} | {}/{} | {}/{} | {} | {} | {:.3} | {:.1} | {:.3} |",
                t.started, t.n, t.phased, t.n, t.early, t.false_events, t.corr / n, t.man_mae / n, t.man_corr / n
            );
        }
    }

    // ---- Real captures ---------------------------------------------------

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

    /// The capture's noise density (per Hz, from the far side of the dial,
    /// -2.5..-1 kHz, where nothing transmits) and the fax's power above it in
    /// +1.3..+2.5 kHz.
    fn capture_snr(fs: f32, iq: &[Complex32]) -> (f64, f64) {
        let n = 4096;
        let fft = rustfft::FftPlanner::<f32>::new().plan_fft_forward(n);
        let win: Vec<f32> = (0..n).map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / n as f32).cos()).collect();
        let wpow: f64 = win.iter().map(|w| (*w as f64).powi(2)).sum();
        let mut psd = vec![0f64; n];
        let mut k = 0;
        for c in iq.chunks_exact(n).step_by(1) {
            let mut x: Vec<Complex32> = c.iter().zip(&win).map(|(a, w)| a * *w).collect();
            fft.process(&mut x);
            for (p, v) in psd.iter_mut().zip(&x) {
                *p += v.norm_sqr() as f64;
            }
            k += 1;
        }
        // Power per Hz: |X|² / (Σw² · fs).
        psd.iter_mut().for_each(|p| *p /= k as f64 * wpow * fs as f64);
        let hz = |i: usize| if i < n / 2 { i as f64 } else { i as f64 - n as f64 } * fs as f64 / n as f64;
        let bin = fs as f64 / n as f64;
        let mut noise: Vec<f64> = (0..n).filter(|&i| (-2500.0..-1000.0).contains(&hz(i))).map(|i| psd[i]).collect();
        noise.sort_by(f64::total_cmp);
        let n0 = noise[noise.len() / 2];
        let sig: f64 = (0..n).filter(|&i| (1300.0..2500.0).contains(&hz(i))).map(|i| (psd[i] - n0) * bin).sum();
        (n0, sig)
    }

    fn decode_capture(fs: f32, iq: &[Complex32]) -> Vec<Vec<u8>> {
        let mut ssb = crate::ssb::SsbDemod::with_passband(fs as f64, crate::ssb::Sideband::Upper, 1000.0, 2800.0);
        let (mut audio, mut linear) = (Vec::new(), Vec::new());
        let mut dec = WefaxDecoder::new(fs);
        let step = (fs * 0.02) as usize;
        let mut ev = Vec::new();
        for (i, chunk) in iq.chunks(step).enumerate() {
            ssb.process_with_linear(chunk, &mut audio, &mut linear);
            dec.push(&linear);
            if i == 0 {
                dec.start_now();
            }
            ev.extend(dec.take_events());
        }
        dec.stop_now();
        ev.extend(dec.take_events());
        ev.into_iter().filter_map(|e| if let WefaxEvent::Line(l) = e { Some(l) } else { None }).collect()
    }

    fn write_pgm(path: &std::path::Path, rows: &[Vec<u8>]) {
        if rows.is_empty() {
            return;
        }
        let w = rows[0].len();
        let mut b = format!("P5 {w} {} 255\n", rows.len()).into_bytes();
        for r in rows {
            b.extend_from_slice(&r[..w.min(r.len())]);
        }
        let _ = std::fs::write(path, b);
    }

    /// The page's median level and the share of pixels darker than mid-gray.
    /// These captures hold a blank stretch of chart (white paper), so every
    /// dark pixel is noise.
    fn page_stats(rows: &[Vec<u8>]) -> (f64, f64) {
        let mut all: Vec<u8> = rows.iter().skip(2).flatten().copied().collect();
        if all.is_empty() {
            return (0.0, 0.0);
        }
        let dark = all.iter().filter(|&&v| v < 128).count() as f64 / all.len() as f64;
        let mid = all.len() / 2;
        let med = *all.select_nth_unstable(mid).1 as f64;
        (med, dark)
    }

    /// Correlation and mean abs error of `got` against `want`, row for row,
    /// past the first two rows.
    fn compare(got: &[Vec<u8>], want: &[Vec<u8>]) -> (f64, f64) {
        let (mut sx, mut sy, mut sxx, mut syy, mut sxy, mut n, mut err) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        for (a, b) in got.iter().zip(want).skip(2) {
            for (&y, &x) in a.iter().zip(b) {
                let (x, y) = (x as f64, y as f64);
                sx += x;
                sy += y;
                sxx += x * x;
                syy += y * y;
                sxy += x * y;
                err += (x - y).abs();
                n += 1.0;
            }
        }
        let corr = (sxy / n - sx / n * sy / n) / ((sxx / n - (sx / n).powi(2)).sqrt() * (syy / n - (sy / n).powi(2)).sqrt()).max(1e-9);
        (corr, err / n)
    }

    /// DWD's charts off the air: `USDR_WEFAX_LIVE=<dir>` with fax1-3-iq.wav.
    /// Each is decoded as captured and with white noise added to bring it
    /// to 5 and 0 dB in 3 kHz; the noisy pictures are scored against the
    /// clean one. Pictures go to `<dir>/out` as PGM.
    #[test]
    fn wefax_live_recordings() {
        let Ok(dir) = std::env::var("USDR_WEFAX_LIVE") else { return };
        let dir = std::path::Path::new(&dir);
        let out = dir.join("out");
        let _ = std::fs::create_dir_all(&out);
        for name in ["fax1", "fax2", "fax3"] {
            let Some((fs, iq)) = read_iq_wav(&dir.join(format!("{name}-iq.wav"))) else {
                println!("{name}: not readable");
                continue;
            };
            let (n0, sig) = capture_snr(fs, &iq);
            let snr = 10.0 * (sig / (n0 * 3000.0)).log10();
            println!("== {name}: {:.1} s at {fs} Hz, {snr:.1} dB in 3 kHz", iq.len() as f32 / fs);
            {
                let clean = decode_capture(fs, &iq);
                write_pgm(&out.join(format!("{name}-clean.pgm")), &clean);
                let (mean, specks) = page_stats(&clean);
                let mut line = format!("  {} rows, paper {mean:.0}, specks {:.2} %;", clean.len(), 100.0 * specks);
                for target in [10.0, 5.0, 0.0] {
                    let mut tot = (0.0, 0.0, 0.0, 0.0);
                    let seeds = 3;
                    for seed in 0..seeds {
                        // Complex noise of variance σ² has density σ²/fs.
                        let want_n0 = sig / (10f64.powf(target / 10.0) * 3000.0);
                        let sigma = ((want_n0 - n0).max(0.0) * fs as f64 / 2.0).sqrt();
                        let mut rng = Rng(0x9e37_79b9 + seed * 7919 + target as u64);
                        let noisy: Vec<Complex32> = iq
                            .iter()
                            .map(|z| z + Complex32::new((sigma * rng.gauss()) as f32, (sigma * rng.gauss()) as f32))
                            .collect();
                        let rows = decode_capture(fs, &noisy);
                        if seed == 0 {
                            write_pgm(&out.join(format!("{name}-{target}dB.pgm")), &rows);
                        }
                        let (c, e) = compare(&rows, &clean);
                        let (m, sp) = page_stats(&rows);
                        tot.0 += c / seeds as f64;
                        tot.1 += e / seeds as f64;
                        tot.2 += m / seeds as f64;
                        tot.3 += sp / seeds as f64;
                    }
                    line += &format!(" | {target} dB: paper {:.0}, specks {:.1} %, corr {:.3}", tot.2, 100.0 * tot.3, tot.0);
                }
                println!("{line}");
            }
        }
    }

    #[test]
    fn wefax_debug_one() {
        let Ok(spec) = std::env::var("USDR_WEFAX_DEBUG") else { return };
        // chan,snr,seed
        let f: Vec<&str> = spec.split(',').collect();
        let chan = match f[0] {
            "awgn" => Chan::Awgn,
            "moderate" => Chan::Moderate,
            "poor" => Chan::Poor,
            _ => Chan::PoorImpulsive,
        };
        let snr: f64 = f[1].parse().unwrap();
        let seed: u64 = f[2].parse().unwrap();
        let pic = Pic::synthetic();
        let w = 1809;
        let mut syn = synth_for(chan, snr, seed);
        let lead = 1.0 + syn.rng.uniform();
        syn.noise(lead);
        syn.tone(5.0, 300.0);
        syn.phasing(20, 120.0, w);
        syn.lines(SWEEP_ROWS, 120.0, w, |r, c| pic.at(r, c));
        syn.tone(5.0, 450.0);
        syn.section(5.0, |_| Some(0));
        syn.noise(2.0);
        println!("lead {lead:.2}: tone {:.2}..{:.2}, phasing to {:.2}, picture to {:.2}", lead, lead + 5.0, lead + 15.0, lead + 15.0 + SWEEP_ROWS as f64 / 2.0);
        let mut dec = WefaxDecoder::new(48_000.0);
        let mut i = 0;
        let mut last = WefaxState::Idle;
        while i < syn.out.len() {
            let n = 960.min(syn.out.len() - i);
            dec.push(&syn.out[i..i + n]);
            i += n;
            let t = i as f64 / 48_000.0;
            for e in dec.take_events() {
                match e {
                    WefaxEvent::Line(_) => {}
                    e => println!("{t:7.2} {e:?}"),
                }
            }
            let st = dec.state();
            if std::mem::discriminant(&st) != std::mem::discriminant(&last) {
                println!("{t:7.2} state {st:?} snr {:?} band {}", dec.carrier_snr_db(), dec.band);
                last = st;
            }
        }
    }

    #[test]
    fn wefax_long_snr_reading() {
        let Ok(_) = std::env::var("USDR_WEFAX_LONGSNR") else { return };
        for chan in [Chan::Awgn, Chan::Moderate, Chan::Poor] {
            for snr in [-100.0, -9.0, -6.0, -3.0, 0.0] {
                let mut lo = f64::MAX;
                let mut hi = f64::MIN;
                for seed in 0..4 {
                    let mut syn = synth_for(chan, snr, seed);
                    if snr < -50.0 {
                        syn.sigma = syn.amp;
                        syn.noise(60.0);
                    } else {
                        let pic = Pic::synthetic();
                        syn.lines(120, 120.0, 1809, |r, c| pic.at(r, c));
                    }
                    let mut dec = WefaxDecoder::new(48_000.0);
                    for (i, c) in syn.out.chunks(4800).enumerate() {
                        dec.push(c);
                        if i > 500 {
                            let v = dec.long_snr_db().unwrap();
                            lo = lo.min(v);
                            hi = hi.max(v);
                        }
                    }
                }
                println!("{chan:?} {snr}: 45 s reading {lo:.1} .. {hi:.1}");
            }
        }
        // Noise through the server's sideband filter, as the decoder gets it.
        let fs = 47_904.0;
        let mut rng = Rng(77);
        let iq: Vec<Complex32> = (0..(70.0 * fs) as usize)
            .map(|_| Complex32::new(rng.gauss() as f32 * 0.01, rng.gauss() as f32 * 0.01))
            .collect();
        let mut ssb = crate::ssb::SsbDemod::with_passband(fs, crate::ssb::Sideband::Upper, 1000.0, 2800.0);
        let (mut audio, mut linear) = (Vec::new(), Vec::new());
        let mut dec = WefaxDecoder::new(fs as f32);
        let (mut lo, mut hi) = (f64::MAX, f64::MIN);
        for (i, c) in iq.chunks(958).enumerate() {
            ssb.process_with_linear(c, &mut audio, &mut linear);
            dec.push(&linear);
            if i > 2500 {
                let v = dec.long_snr_db().unwrap();
                lo = lo.min(v);
                hi = hi.max(v);
            }
        }
        println!("sideband noise: 45 s reading {lo:.1} .. {hi:.1}, short {:?}, band {}", dec.carrier_snr_db(), dec.band);
    }

    /// The two-second tone share and hit count on material that has no
    /// tone: noise in several channels, the synthetic picture, and a real
    /// chart (`USDR_WEFAX_CHART`), received with `start_now`. Gated by
    /// `USDR_WEFAX_FALSE=<minutes>`.
    #[test]
    fn wefax_false_tone_margin() {
        let Ok(mins) = std::env::var("USDR_WEFAX_FALSE") else { return };
        let mins: f64 = mins.parse().unwrap_or(5.0);
        let chart = Pic::load();
        let synth = Pic::synthetic();
        let cases: Vec<(&str, Chan, f64, Option<&Pic>)> = vec![
            ("noise", Chan::Awgn, -100.0, None),
            ("noise+impulses", Chan::PoorImpulsive, -100.0, None),
            ("synthetic picture 20 dB", Chan::Awgn, 20.0, Some(&synth)),
            ("synthetic picture 0 dB poor", Chan::Poor, 0.0, Some(&synth)),
            ("chart 25 dB", Chan::Awgn, 25.0, Some(&chart)),
            ("chart 10 dB moderate", Chan::Moderate, 10.0, Some(&chart)),
            ("chart 0 dB poor", Chan::Poor, 0.0, Some(&chart)),
        ];
        std::thread::scope(|sc| {
            let hs: Vec<_> = cases
                .into_iter()
                .map(|(name, chan, snr, pic)| {
                    sc.spawn(move || {
                        let mut syn = synth_for(chan, snr, 5);
                        let secs = mins * 60.0;
                        match pic {
                            None => {
                                syn.sigma = syn.amp;
                                syn.noise(secs);
                            }
                            Some(p) => syn.lines((secs * 2.0) as usize, 120.0, 1809, |r, c| p.at(r, c)),
                        }
                        let mut dec = WefaxDecoder::new(48_000.0);
                        if pic.is_some() {
                            dec.start_now();
                        }
                        let mut worst = [(0f64, 0usize); 3];
                        let mut events = Vec::new();
                        for c in syn.out.chunks(960) {
                            dec.push(c);
                            events.extend(dec.take_events().into_iter().filter(|e| !matches!(e, WefaxEvent::Line(_))));
                            if dec.ring.len() == TONE_RING {
                                let var: f64 = dec.ring.iter().map(|b| b.var).sum();
                                for (k, wk) in worst.iter_mut().enumerate() {
                                    let share = dec.ring.iter().map(|b| b.power[k]).sum::<f64>() / var;
                                    let hits = dec.ring.iter().filter(|b| b.share(k) > BLOCK_SHARE).count();
                                    wk.0 = wk.0.max(share);
                                    wk.1 = wk.1.max(hits);
                                }
                            }
                        }
                        let ev: Vec<String> = events.iter().map(|e| format!("{e:?}")).collect();
                        format!(
                            "{name}: {mins} min; max 2 s share / hits: 300 Hz {:.4}/{}, 450 Hz {:.4}/{}, 675 Hz {:.4}/{}; events {}",
                            worst[0].0, worst[0].1, worst[1].0, worst[1].1, worst[2].0, worst[2].1, ev.join(" ")
                        )
                    })
                })
                .collect();
            for h in hs {
                println!("{}", h.join().unwrap());
            }
        });
    }

    /// A start tone straight into a real chart (`USDR_WEFAX_CHART`), no
    /// phasing: nothing in the chart may pass for phasing lines.
    #[test]
    fn wefax_chart_not_phasing() {
        if std::env::var_os("USDR_WEFAX_CHART").is_none() {
            return;
        }
        let pic = Pic::load();
        for (seed, start_row) in [(1u64, 0usize), (2, 150), (3, 300), (4, 450)] {
            for snr in std::env::var("USDR_WEFAX_CHART_SNR").ok().map(|v| vec![v.parse().unwrap()]).unwrap_or(vec![25.0, 5.0]) {
                let mut syn = synth_for(Chan::Awgn, snr, seed);
                syn.noise(1.0);
                syn.tone(5.0, 300.0);
                syn.lines(120, 120.0, 1809, |r, c| pic.at(r + start_row, c));
                syn.tone(5.0, 450.0);
                let mut dec = WefaxDecoder::new(48_000.0);
                let ev = run(&mut dec, &syn.out, seed);
                println!("chart from row {start_row} at {snr} dB: {} {} rows", kinds(&ev), rows(&ev).len());
                assert_eq!(kinds(&ev), "SLE");
            }
        }
    }
}
