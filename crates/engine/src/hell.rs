//! Feld Hell (Rudolf Hell, 1929; the amateur Feld-Hellschreiber), received
//! as a picture: the decoder paints what was sent and the operator reads
//! it, as on the original paper strip. There is no character recognition.
//!
//! The transmitter keys a carrier on and off. Each character is a cell of
//! 7 columns (the glyph and its spacing) by 14 pixels, sent a column at a
//! time, bottom to top, at 17.5 columns a second: 245 pixels a second,
//! 4.08 ms a pixel. Fonts draw with pixels in pairs, so the fastest keying
//! is 122.5 baud, the figure usually quoted. Each column is sent once.
//! (The old receivers printed the strip twice, one copy above the other,
//! so that text still read whole when the receiver's column phase cut a
//! line of characters in two, or when a clock error slanted it: that is
//! the display's job, and the [`HellEvent::Columns`] stream is meant to be
//! painted that way.)
//!
//! The audio is taken to 8 kHz. An averaged spectrum finds the strongest
//! carrier between 300 and 2700 Hz; it is mixed to zero, filtered to
//! 150 Hz either side (a pixel's keying, little noise) and its envelope
//! taken. Pixels are the mean envelope over each pixel's exact time span,
//! scaled between a noise floor and a peak level that both follow the
//! signal, so a fading signal keeps its contrast.
//!
//! The column phase is the receiver's choice: nothing in the signal marks
//! where a column starts. Text sits in the middle of the cell's height
//! with blank rows above and below it, so the decoder keeps a profile of
//! where in the column the ink falls and moves its column clock to put the
//! ink in the middle. Followed slowly, the same correction cancels a
//! difference between the transmitter's clock and this receiver's, which
//! would otherwise slant the text; the correction rate is reported as the
//! clock error.
//!
//! Measured on the test channel: the decoder locks on Feld Hell text from
//! -6 dB SNR in 2500 Hz (counting the carrier at full strength), and
//! with the transmitter's clock 100 or 1000 ppm off the text holds its row
//! within a pixel from start to end, the clock error read within 10%.
//! Other Hell modes (FSK Hell, Hell 80, Slow Hell, Hell X5 and X9) are not
//! received.

use crate::mfsk::resample::Resampler;
use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::f32::consts::TAU;
use std::sync::Arc;

/// The rate the receiver works at.
const RATE: f32 = 8000.0;
/// Pixels in a column.
pub const HELL_ROWS: usize = 14;
/// Columns a second.
const COLUMN_RATE: f64 = 17.5;
/// The envelope's rate: the 8 kHz stream decimated by this.
const DECIM: usize = 4;
/// Quarter pixels: the column profile's resolution.
const SUB: usize = 4;
const PROFILE: usize = HELL_ROWS * SUB;

/// Spectrum for the carrier search: FFT length and hop at 8 kHz.
const SPEC_N: usize = 1024;
const SPEC_HOP: usize = 512;
/// Averaging time of the spectrum.
const SPEC_TAU_S: f32 = 2.0;
/// The search looks again this often.
const SEARCH_EVERY_S: f32 = 0.5;
/// Where the carrier may be.
const LO_HZ: f32 = 300.0;
const HI_HZ: f32 = 2700.0;
/// The averaged power within 150 Hz of the carrier must be this many times
/// the noise there, in two looks running, to lock. Noise alone, the best of
/// every position in the band, stays below 1.15 (the spectrum averages
/// some 30 spectra over 38 bins); text reaches this at about -6 dB in
/// 2500 Hz (the carrier at full strength)...
const LOCK_RATIO: f32 = 1.4;
/// ...and is let go after this long below this.
const UNLOCK_RATIO: f32 = 1.2;
const UNLOCK_S: f32 = 5.0;
/// The carrier estimate moves this share of the way to each look's, and
/// the mixer follows once it has moved this far.
const CENTER_GAIN: f32 = 0.3;
const RETUNE_HZ: f32 = 4.0;
/// One-sided width of the baseband filter.
const BB_HZ: f32 = 150.0;

/// The keying's edges are averaged over about this many columns (1.1 s)...
const EDGE_TAU_COLS: f32 = 20.0;
/// ...and steer the pixel clock once they are this clearly placed (first
/// harmonic against the sum; noise alone gives about 0.02)...
const EDGE_SURE: f32 = 0.06;
/// ...by this share of the error each column, and its rate by this share.
const PIXEL_GAIN: f64 = 0.1;
const RATE_GAIN: f64 = 0.004;
/// The clock error is the rate averaged over about this many columns (11
/// s), and reported once the pixel clock has followed the edges that long.
const DRIFT_AFTER_COLS: u64 = 200;
/// The ink profile is a plain mean over the first this many columns, which
/// is when the column phase is first set...
const PROFILE_FIRST_COLS: f32 = 60.0;
/// ...and then forgets over about this many (23 s): the text itself moves
/// the ink up and down from one word to the next, which the column phase
/// must not follow; the pixel clock keeps it from slipping.
const PROFILE_TAU_COLS: f32 = 400.0;
/// The column phase moves once the profile's ink is this lopsided (first
/// harmonic against the mean).
const PROFILE_SURE: f32 = 0.06;

/// The peak level falls back over about this long; the noise floor rises
/// over about this long.
const PEAK_DECAY_S: f32 = 3.0;
const FLOOR_RISE_S: f32 = 2.0;
/// Both follow a change toward them over about this long.
const LEVEL_FAST_S: f32 = 0.05;

#[derive(Clone, Debug, PartialEq)]
pub enum HellEvent {
    /// Columns received since the last event, `HELL_ROWS` pixels each,
    /// column after column; within a column the top row first. 0 is no
    /// carrier, 255 the carrier at full strength.
    Columns { pixels: Vec<u8> },
}

impl HellEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            HellEvent::Columns { .. } => "columns",
        }
    }

    pub fn summary(&self) -> String {
        match self {
            HellEvent::Columns { pixels } => format!("{} columns", pixels.len() / HELL_ROWS),
        }
    }

    pub fn fields(&self) -> Vec<(&'static str, String)> {
        match self {
            HellEvent::Columns { pixels } => vec![
                ("rows", HELL_ROWS.to_string()),
                ("columns", (pixels.len() / HELL_ROWS).to_string()),
                (
                    "pixels",
                    pixels.iter().map(|p| format!("{p:02x}")).collect(),
                ),
            ],
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct HellStatus {
    /// A carrier is held and columns are coming out.
    pub locked: bool,
    pub center_hz: Option<f32>,
    /// Mean signal power against the noise in 2500 Hz, from the averaged
    /// spectrum.
    pub snr_db: Option<f32>,
    /// How much longer the transmitter's columns are than they should be,
    /// in parts per million, as the column clock has followed it (the
    /// difference between its clock and this receiver's). None until the
    /// clock has followed text for a while.
    pub drift_ppm: Option<f32>,
    /// Columns sent out since the decoder was made.
    pub columns: u64,
}

/// The carrier search.
struct Search {
    fft: Arc<dyn Fft<f32>>,
    win: Vec<f32>,
    buf: Vec<Complex32>,
    scratch: Vec<Complex32>,
    ring: Vec<f32>,
    pos: usize,
    since: usize,
    avg: Vec<f32>,
    frames: u32,
    alpha: f32,
}

impl Search {
    fn new() -> Self {
        let fft = FftPlanner::new().plan_fft_forward(SPEC_N);
        let scratch = vec![Complex32::new(0.0, 0.0); fft.get_inplace_scratch_len()];
        let win = (0..SPEC_N)
            .map(|i| 0.5 - 0.5 * (TAU * i as f32 / SPEC_N as f32).cos())
            .collect();
        let frames_per_s = RATE / SPEC_HOP as f32;
        Self {
            fft,
            win,
            buf: vec![Complex32::new(0.0, 0.0); SPEC_N],
            scratch,
            ring: vec![0.0; SPEC_N],
            pos: 0,
            since: 0,
            avg: vec![0.0; SPEC_N / 2],
            frames: 0,
            alpha: 1.0 / (SPEC_TAU_S * frames_per_s),
        }
    }

    /// One sample; true when a new spectrum went into the average.
    fn push(&mut self, x: f32) -> bool {
        self.ring[self.pos] = x;
        self.pos = (self.pos + 1) % SPEC_N;
        self.since += 1;
        if self.since < SPEC_HOP {
            return false;
        }
        self.since = 0;
        for i in 0..SPEC_N {
            self.buf[i] = Complex32::new(self.ring[(self.pos + i) % SPEC_N] * self.win[i], 0.0);
        }
        self.fft
            .process_with_scratch(&mut self.buf, &mut self.scratch);
        let a = if self.frames < 8 {
            1.0 / (self.frames + 1) as f32
        } else {
            self.alpha
        };
        for (v, z) in self.avg.iter_mut().zip(&self.buf) {
            *v += a * (z.norm_sqr() - *v);
        }
        self.frames += 1;
        true
    }

    /// The strongest Hell signal in the band: its carrier frequency, the
    /// power within the baseband filter's reach against the noise there
    /// (a ratio, 1 for noise alone), and the SNR in 2500 Hz.
    ///
    /// The power is summed over the keying sidebands, not taken from the
    /// carrier line alone: text keys the carrier on a quarter of the time
    /// or so, which leaves only a sixteenth of the power in the line.
    fn best(&self) -> Option<(f32, f32, f32)> {
        let bin_hz = RATE / SPEC_N as f32;
        let lo = (LO_HZ / bin_hz).ceil() as usize;
        let hi = (HI_HZ / bin_hz).floor() as usize;
        let mut band: Vec<f32> = self.avg[lo..=hi].to_vec();
        let mid = band.len() / 2;
        band.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
        // Averaged over a few dozen spectra, a noise bin's power is close
        // to its mean, and so is the median.
        let floor = band[mid];
        if floor <= 0.0 {
            return None;
        }
        let reach = (BB_HZ / bin_hz).round() as usize;
        let width = (2 * reach + 1) as f32;
        let sum = |k: usize| -> f32 { self.avg[k - reach..=k + reach].iter().sum() };
        let (k, total) = (lo + reach..=hi - reach)
            .map(|k| (k, sum(k)))
            .max_by(|a, b| a.1.total_cmp(&b.1))?;
        // The keying's sidebands sit evenly either side of the carrier:
        // the middle of the power above the noise there is the carrier.
        // The window is centered again on each answer, since one off center
        // cuts one side's tail more than the other's.
        let mut mid = k as f32;
        for _ in 0..4 {
            let c = (mid.round() as usize).clamp(lo + reach, hi - reach);
            let (mut w, mut m) = (0.0f32, 0.0f32);
            for j in c - reach..=c + reach {
                let e = (self.avg[j] - floor).max(0.0);
                w += e;
                m += e * j as f32;
            }
            if w > 0.0 {
                mid = m / w;
            }
        }
        // The carrier line itself, when it stands clear, a little closer.
        let near = (10.0 / bin_hz).round() as usize;
        let c0 = mid.round() as usize;
        let (c, peak) = (c0 - near..=c0 + near)
            .map(|j| (j, self.avg[j]))
            .max_by(|a, b| a.1.total_cmp(&b.1))?;
        let hz = if peak > 4.0 * floor {
            let (l, r) = (self.avg[c - 1], self.avg[c + 1]);
            let den = l - 2.0 * peak + r;
            let frac = if den < 0.0 {
                (0.5 * (l - r) / den).clamp(-0.5, 0.5)
            } else {
                0.0
            };
            (c as f32 + frac) * bin_hz
        } else {
            mid * bin_hz
        };
        let ratio = total / (floor * width);
        // Hann-window bins carry 1.5 bins of noise bandwidth each.
        let sig = (total - floor * width).max(1e-20);
        let snr = 10.0 * (sig / (floor * 2500.0 / (1.5 * bin_hz))).log10();
        Some((hz, ratio, snr))
    }
}

/// Mixer, filter and decimator to the envelope.
struct Front {
    phase: f32,
    step: f32,
    taps: Vec<f32>,
    hist: Vec<Complex32>,
    pos: usize,
    count: usize,
}

impl Front {
    fn new(hz: f32) -> Self {
        // A Blackman-windowed lowpass: 150 Hz, about 100 Hz of transition
        // at 8 kHz, which also clears the image at twice the carrier.
        let n = 241;
        let fc = BB_HZ / RATE;
        let mut taps: Vec<f32> = (0..n)
            .map(|i| {
                let x = i as f32 - (n - 1) as f32 / 2.0;
                let s = if x == 0.0 {
                    2.0 * fc
                } else {
                    (TAU * fc * x).sin() / (std::f32::consts::PI * x)
                };
                let u = i as f32 / (n - 1) as f32;
                s * (0.42 - 0.5 * (TAU * u).cos() + 0.08 * (2.0 * TAU * u).cos())
            })
            .collect();
        let sum: f32 = taps.iter().sum();
        taps.iter_mut().for_each(|t| *t /= sum);
        Self {
            phase: 0.0,
            step: TAU * hz / RATE,
            taps,
            hist: vec![Complex32::new(0.0, 0.0); n],
            pos: 0,
            count: 0,
        }
    }

    fn retune(&mut self, hz: f32) {
        self.step = TAU * hz / RATE;
    }

    /// One sample; the envelope every `DECIM` samples.
    fn push(&mut self, x: f32) -> Option<f32> {
        let z = Complex32::new(self.phase.cos(), -self.phase.sin()) * x;
        self.phase = (self.phase + self.step) % TAU;
        self.hist[self.pos] = z;
        self.pos = (self.pos + 1) % self.hist.len();
        self.count += 1;
        if self.count < DECIM {
            return None;
        }
        self.count = 0;
        let n = self.hist.len();
        let mut acc = Complex32::new(0.0, 0.0);
        for (i, t) in self.taps.iter().enumerate() {
            acc += self.hist[(self.pos + i) % n] * *t;
        }
        // Twice the baseband magnitude is the carrier's amplitude.
        Some(2.0 * acc.norm())
    }
}

pub struct HellDecoder {
    resampler: Resampler,
    tmp: Vec<f32>,
    search: Search,
    search_every: u32,
    front: Option<Front>,
    center: Option<f32>,
    /// Where the mixer is.
    tuned: f32,
    snr_db: Option<f32>,
    /// Looks in a row with the signal above [`LOCK_RATIO`].
    seen: u32,
    /// Seconds below [`UNLOCK_RATIO`].
    weak_s: f32,
    clock: Clock,
    columns: u64,
    pixels: Vec<u8>,
    events: Vec<HellEvent>,
}

/// The pixel and column clock, the levels, and the column being built.
struct Clock {
    env_rate: f64,
    /// Envelope samples per quarter pixel, nominally.
    quarter: f64,
    /// Where the current quarter pixel ends, in envelope samples.
    next_edge: f64,
    /// Which quarter of its pixel the current one is.
    quarter_in_pixel: usize,
    /// Envelope samples taken in since lock.
    n: u64,
    acc: f32,
    acc_w: f32,
    prev: f32,
    /// Where in the pixel the envelope changes: the first harmonic of its
    /// slope, folded on the pixel grid, and the slope's sum.
    edges: Complex32,
    edge_sum: f32,
    peak: f32,
    floor: f32,
    /// Quarter pixels of the column being built, bottom first.
    quarters: Vec<f32>,
    /// Quarter pixels still to drop, to start a column later.
    skip: usize,
    profile: [f32; PROFILE],
    centered: bool,
    /// Columns the profile has taken in.
    profiled: u64,
    /// The pixel clock's learned correction per column (envelope
    /// samples), and how many columns it has learned from.
    rate: f64,
    rate_cols: u64,
    /// The rate averaged over the last [`DRIFT_AFTER_COLS`] or so, once it
    /// has settled.
    drift: f64,
}

impl Clock {
    fn new() -> Self {
        let env_rate = (RATE as usize / DECIM) as f64;
        let quarter = env_rate / (COLUMN_RATE * (HELL_ROWS * SUB) as f64);
        Self {
            env_rate,
            quarter,
            next_edge: quarter,
            quarter_in_pixel: 0,
            n: 0,
            acc: 0.0,
            acc_w: 0.0,
            prev: 0.0,
            edges: Complex32::new(0.0, 0.0),
            edge_sum: 0.0,
            peak: 0.0,
            floor: 0.0,
            quarters: Vec::with_capacity(PROFILE),
            skip: 0,
            profile: [0.0; PROFILE],
            centered: false,
            profiled: 0,
            rate: 0.0,
            rate_cols: 0,
            drift: 0.0,
        }
    }

    /// One envelope sample: note where it changes on the pixel grid, and
    /// spread it over the quarter pixels it overlaps. Finished columns go
    /// to `out`, top row first.
    fn push(&mut self, e: f32, out: &mut Vec<u8>) -> u64 {
        let t0 = self.n as f64;
        let t1 = t0 + 1.0;
        self.n += 1;
        // The slope between the last sample and this one sits half a
        // sample back. Squared, so the keying's edges stand out of the
        // noise's small wiggles.
        let d = (e - self.prev).powi(2);
        self.prev = e;
        let into = (t0 - 0.5 - (self.next_edge - self.quarter)) / self.quarter;
        let p = (self.quarter_in_pixel as f64 + into) / SUB as f64;
        self.edges += Complex32::from_polar(d, (std::f64::consts::TAU * p) as f32);
        self.edge_sum += d;

        let mut cols = 0;
        let mut t = t0;
        while self.next_edge < t1 {
            let w = (self.next_edge - t).max(0.0) as f32;
            self.acc += w * e;
            self.acc_w += w;
            let q = if self.acc_w > 0.0 {
                self.acc / self.acc_w
            } else {
                e
            };
            t = self.next_edge.max(t);
            self.acc = 0.0;
            self.acc_w = 0.0;
            self.next_edge += self.quarter;
            self.quarter_in_pixel = (self.quarter_in_pixel + 1) % SUB;
            if self.quarter_done(q, out) {
                cols += 1;
            }
        }
        let w = (t1 - t) as f32;
        self.acc += w * e;
        self.acc_w += w;
        cols
    }

    /// A quarter pixel's mean envelope; true when it finished a column.
    fn quarter_done(&mut self, q: f32, out: &mut Vec<u8>) -> bool {
        // Levels: the peak follows the signal up quickly and decays slowly;
        // the floor follows it down quickly and rises slowly.
        let dt = (self.quarter / self.env_rate) as f32;
        let fast = (dt / LEVEL_FAST_S).min(1.0);
        if self.peak == 0.0 && self.floor == 0.0 {
            self.peak = q;
            self.floor = q;
        }
        let up = if q > self.peak {
            fast
        } else {
            dt / PEAK_DECAY_S
        };
        self.peak += up * (q - self.peak);
        let down = if q < self.floor {
            fast
        } else {
            dt / FLOOR_RISE_S
        };
        self.floor += down * (q - self.floor);
        if self.skip > 0 {
            self.skip -= 1;
            return false;
        }
        let span = (self.peak - self.floor).max(1e-12);
        self.quarters
            .push(((q - self.floor) / span).clamp(0.0, 1.0));
        if self.quarters.len() < PROFILE {
            return false;
        }
        self.column_done(out);
        true
    }

    fn column_done(&mut self, out: &mut Vec<u8>) {
        // Pixel phase: put the keying's edges on the pixel boundaries. This
        // also follows a clock error, a little at a time.
        let edge_k = 1.0 / EDGE_TAU_COLS;
        let pixel = SUB as f64 * self.quarter;
        let mut shift = self.rate;
        if self.edge_sum > 0.0 && self.edges.norm() / self.edge_sum > EDGE_SURE {
            let err = self.edges.arg() as f64 / std::f64::consts::TAU;
            // A second-order loop: the rate term learns the clock error, so
            // the pixels sit on the edges without a standing lag.
            self.rate += RATE_GAIN * err * pixel;
            shift = PIXEL_GAIN * err * pixel + self.rate;
            self.rate_cols += 1;
            let k = 1.0 / (self.rate_cols as f64).min(DRIFT_AFTER_COLS as f64);
            if self.rate_cols > DRIFT_AFTER_COLS / 4 {
                self.drift += k * (self.rate - self.drift);
            }
        }
        self.next_edge += shift;
        self.edges *= Complex32::from_polar(1.0, -(std::f64::consts::TAU * shift / pixel) as f32);
        self.edges *= 1.0 - edge_k;
        self.edge_sum *= 1.0 - edge_k;

        // Column phase: where the ink falls, in whole pixels from the
        // middle. Ink late in the column means the column started early.
        self.profiled += 1;
        let k = 1.0 / (self.profiled as f32).min(PROFILE_TAU_COLS);
        for (p, q) in self.profile.iter_mut().zip(&self.quarters) {
            *p += k * (q - *p);
        }
        let mut c = Complex32::new(0.0, 0.0);
        let mut sum = 0.0;
        for (b, p) in self.profile.iter().enumerate() {
            c += Complex32::from_polar(*p, TAU * b as f32 / PROFILE as f32);
            sum += p;
        }
        let mut step = 0i64;
        if self.rate_cols > 0
            && self.profiled >= PROFILE_FIRST_COLS as u64
            && sum > 0.0
            && c.norm() / sum > PROFILE_SURE
        {
            let mut ang = c.arg();
            if ang < 0.0 {
                ang += TAU;
            }
            let err = (ang - std::f32::consts::PI) / TAU * HELL_ROWS as f32;
            // Once centered, move only for an error of a pixel and a half (a
            // slip), so the column does not dither between two phases.
            if !self.centered || err.abs() >= 1.5 {
                step = err.round() as i64;
                self.centered = true;
            }
        }

        // Out with the column, top row first. A column started later drops
        // the quarters that would have begun the next one; one started
        // earlier ends this one short, blank at the top.
        let keep = if step < 0 {
            PROFILE - SUB * (-step) as usize
        } else {
            PROFILE
        };
        let mut col = [0u8; HELL_ROWS];
        for (r, px) in col.iter_mut().enumerate() {
            if (r + 1) * SUB <= keep {
                let v: f32 = self.quarters[r * SUB..(r + 1) * SUB].iter().sum::<f32>() / SUB as f32;
                *px = (v * 255.0).round() as u8;
            }
        }
        col.reverse();
        out.extend_from_slice(&col);
        let carry: Vec<f32> = self.quarters[keep..].to_vec();
        self.quarters.clear();
        self.quarters.extend_from_slice(&carry);
        if step > 0 {
            self.skip = SUB * step as usize;
        }
        if step != 0 {
            let by = SUB as i64 * step;
            let old = self.profile;
            for (b, p) in self.profile.iter_mut().enumerate() {
                *p = old[(b as i64 + by).rem_euclid(PROFILE as i64) as usize];
            }
        }
    }
}

impl HellDecoder {
    pub fn new(fs: f32) -> Self {
        let frames_per_s = RATE / SPEC_HOP as f32;
        Self {
            resampler: Resampler::new(fs, RATE, 3000.0, 5000.0),
            tmp: Vec::with_capacity(4096),
            search: Search::new(),
            search_every: (SEARCH_EVERY_S * frames_per_s).round().max(1.0) as u32,
            front: None,
            center: None,
            tuned: 0.0,
            snr_db: None,
            seen: 0,
            weak_s: 0.0,
            clock: Clock::new(),
            columns: 0,
            pixels: Vec::new(),
            events: Vec::new(),
        }
    }

    pub fn push(&mut self, audio: &[f32]) {
        self.tmp.clear();
        self.resampler.process(audio, &mut self.tmp);
        let tmp = std::mem::take(&mut self.tmp);
        for &x in &tmp {
            if self.search.push(x) && self.search.frames.is_multiple_of(self.search_every) {
                self.look();
            }
            if let Some(e) = self.front.as_mut().and_then(|f| f.push(x)) {
                self.columns += self.clock.push(e, &mut self.pixels);
            }
        }
        self.tmp = tmp;
        if !self.pixels.is_empty() {
            self.events.push(HellEvent::Columns {
                pixels: std::mem::take(&mut self.pixels),
            });
        }
    }

    pub fn take(&mut self) -> Vec<HellEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn status(&self) -> HellStatus {
        let c = &self.clock;
        HellStatus {
            locked: self.front.is_some(),
            center_hz: self.center,
            snr_db: self.snr_db,
            drift_ppm: (self.front.is_some() && c.rate_cols > DRIFT_AFTER_COLS).then(|| {
                let col = c.env_rate / COLUMN_RATE;
                (c.drift / col * 1e6) as f32
            }),
            columns: self.columns,
        }
    }

    /// Look at the averaged spectrum: lock, follow, or let go.
    fn look(&mut self) {
        let Some((hz, ratio, snr)) = self.search.best() else {
            return;
        };
        let dt = self.search_every as f32 * SPEC_HOP as f32 / RATE;
        match self.front.as_mut() {
            None => {
                if ratio >= LOCK_RATIO {
                    self.seen += 1;
                } else {
                    self.seen = 0;
                }
                if self.seen >= 2 {
                    self.front = Some(Front::new(hz));
                    self.center = Some(hz);
                    self.tuned = hz;
                    self.snr_db = Some(snr);
                    self.weak_s = 0.0;
                    self.clock = Clock::new();
                }
            }
            Some(front) => {
                // Each look's estimate is rough on a weak signal: average
                // them, and move the mixer when the average has moved.
                let center = self.center.unwrap_or(hz);
                if ratio >= UNLOCK_RATIO && (hz - center).abs() < 3.0 * BB_HZ {
                    let c = center + CENTER_GAIN * (hz - center);
                    self.center = Some(c);
                    if (c - self.tuned).abs() > RETUNE_HZ {
                        front.retune(c);
                        self.tuned = c;
                    }
                }
                if ratio < UNLOCK_RATIO {
                    self.weak_s += dt;
                    if self.weak_s >= UNLOCK_S {
                        self.front = None;
                        self.seen = 0;
                        self.snr_db = None;
                    }
                } else {
                    self.weak_s = 0.0;
                    self.snr_db = Some(snr);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtty::testsig::{Rng, add_noise, chunks, rayleigh};

    /// A 5 by 7 font for the test messages, rows top to bottom, bit 4 the
    /// leftmost column.
    fn glyph(c: char) -> [u8; 7] {
        match c {
            'C' => [0x0e, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0e],
            'Q' => [0x0e, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0d],
            'D' => [0x1e, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1e],
            'E' => [0x1f, 0x10, 0x10, 0x1e, 0x10, 0x10, 0x1f],
            'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x15, 0x0a],
            'A' => [0x0e, 0x11, 0x11, 0x1f, 0x11, 0x11, 0x11],
            'H' => [0x11, 0x11, 0x11, 0x1f, 0x11, 0x11, 0x11],
            'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1f],
            'T' => [0x1f, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
            'S' => [0x0f, 0x10, 0x10, 0x0e, 0x01, 0x01, 0x1e],
            '1' => [0x04, 0x0c, 0x04, 0x04, 0x04, 0x04, 0x0e],
            '0' => [0x0e, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0e],
            _ => [0; 7],
        }
    }

    /// Columns of a message, 14 pixels each, bottom first, 7 columns a
    /// character (5 of glyph, 2 of space). The 5 by 7 glyph's inner five
    /// rows are doubled and its top and bottom rows single, 12 pixels in
    /// all, which leaves a blank pixel above and below, as Feld Hell fonts
    /// leave a margin.
    fn columns(text: &str) -> Vec<[bool; HELL_ROWS]> {
        let mut out = Vec::new();
        for c in text.chars() {
            let g = glyph(c);
            for x in 0..7 {
                let mut col = [false; HELL_ROWS];
                if x < 5 {
                    for (row, bits) in g.iter().enumerate() {
                        let on = (bits >> (4 - x)) & 1 == 1;
                        // Glyph row 0 is the top; pixel 0 the bottom.
                        let (lo, hi) = match row {
                            0 => (12, 13),
                            6 => (1, 2),
                            r => (12 - 2 * r, 14 - 2 * r),
                        };
                        for p in col.iter_mut().take(hi).skip(lo) {
                            *p = on;
                        }
                    }
                }
                out.push(col);
            }
        }
        out
    }

    /// On-off keyed carrier with 1 ms raised-cosine edges. `ppm` stretches
    /// the transmitter's clock. Mean power while keyed on is that of a
    /// sine of `amp`.
    fn hell_tx(cols: &[[bool; HELL_ROWS]], fs: f32, hz: f32, ppm: f64, amp: f32) -> Vec<f32> {
        let px_s = 1.0 / (COLUMN_RATE * HELL_ROWS as f64) * (1.0 + ppm * 1e-6);
        let bits: Vec<bool> = cols.iter().flat_map(|c| c.iter().copied()).collect();
        let lead = fs as usize;
        let n = lead + (bits.len() as f64 * px_s * fs as f64) as usize + lead;
        let edge = 0.001;
        (0..n)
            .map(|i| {
                let t = (i as f64 - lead as f64) / fs as f64;
                if t < 0.0 {
                    return 0.0;
                }
                let k = (t / px_s) as usize;
                if k >= bits.len() {
                    return 0.0;
                }
                // Distance into the pixel and the envelope's ramps.
                let on = |j: usize| j < bits.len() && bits[j];
                let into = t - k as f64 * px_s;
                let mut a = if on(k) { 1.0 } else { 0.0 };
                if into < edge / 2.0 && k > 0 && on(k - 1) != on(k) {
                    let u = 0.5 + into / edge;
                    let r = 0.5 - 0.5 * (std::f64::consts::PI * u).cos();
                    a = if on(k) { r } else { 1.0 - r };
                } else if px_s - into < edge / 2.0 && on(k + 1) != on(k) {
                    let u = 0.5 - (px_s - into) / edge;
                    let r = 0.5 - 0.5 * (std::f64::consts::PI * u).cos();
                    a = if on(k) { 1.0 - r } else { r };
                }
                (amp as f64 * a * (std::f64::consts::TAU * hz as f64 * i as f64 / fs as f64).sin())
                    as f32
            })
            .collect()
    }

    fn decode(x: &[f32], fs: f32) -> (Vec<[u8; HELL_ROWS]>, HellStatus) {
        let mut dec = HellDecoder::new(fs);
        let mut cols = Vec::new();
        chunks(x, |c| {
            dec.push(c);
            for e in dec.take() {
                let HellEvent::Columns { pixels } = e;
                for c in pixels.chunks(HELL_ROWS) {
                    cols.push(c.try_into().unwrap());
                }
            }
        });
        (cols, dec.status())
    }

    /// Sent columns, top row first, as the decoder puts them out.
    fn top_first(sent: &[[bool; HELL_ROWS]]) -> Vec<[bool; HELL_ROWS]> {
        sent.iter()
            .map(|c| {
                let mut t = *c;
                t.reverse();
                t
            })
            .collect()
    }

    /// Pixels that agree between received columns from `lag` on and sent
    /// columns `from..to`, the received ones moved up `shift` rows.
    fn agree(
        got: &[[u8; HELL_ROWS]],
        sent: &[[bool; HELL_ROWS]],
        from: usize,
        to: usize,
        lag: i64,
        shift: usize,
    ) -> usize {
        let mut ok = 0;
        for (i, s) in sent.iter().enumerate().take(to).skip(from) {
            let j = lag + i as i64;
            if j < 0 {
                continue;
            }
            let Some(g) = got.get(j as usize) else { break };
            ok += (0..HELL_ROWS)
                .filter(|&r| (g[(r + shift) % HELL_ROWS] > 127) == s[r])
                .count();
        }
        ok
    }

    /// The best column lag (within `lags`) and row shift for sent columns
    /// `from..to`, and the share of pixels that then agree.
    fn align(
        got: &[[u8; HELL_ROWS]],
        sent: &[[bool; HELL_ROWS]],
        from: usize,
        to: usize,
        lags: std::ops::RangeInclusive<i64>,
    ) -> (i64, usize, f32) {
        let mut best = (0usize, 0i64, 0usize);
        for lag in lags {
            for shift in 0..HELL_ROWS {
                let ok = agree(got, sent, from, to, lag, shift);
                if ok > best.0 {
                    best = (ok, lag, shift);
                }
            }
        }
        (
            best.1,
            best.2,
            best.0 as f32 / ((to - from) * HELL_ROWS) as f32,
        )
    }

    /// How far down (rows, signed) the received text sits against the sent
    /// text, in each stretch of `per` sent columns from `from`: the whole
    /// rows of the best alignment plus the difference of the ink's mean
    /// row after it.
    fn offsets(
        got: &[[u8; HELL_ROWS]],
        sent: &[[bool; HELL_ROWS]],
        from: usize,
        per: usize,
    ) -> Vec<f32> {
        let (lag0, _, _) = align(
            got,
            sent,
            from,
            from + per,
            -(from as i64)..=got.len() as i64 - from as i64,
        );
        let mut out = Vec::new();
        let mut k = from;
        while k + per <= sent.len() {
            let (lag, shift, frac) = align(got, sent, k, k + per, lag0 - 3..=lag0 + 3);
            if frac > 0.9 {
                let (mut gw, mut gs, mut sw, mut ss) = (0.0, 0.0, 0.0, 0.0);
                for (i, s) in sent.iter().enumerate().take(k + per).skip(k) {
                    let Some(g) = got.get((lag + i as i64) as usize) else {
                        break;
                    };
                    for r in 0..HELL_ROWS {
                        let v = g[(r + shift) % HELL_ROWS] as f32 / 255.0;
                        gw += v;
                        gs += v * r as f32;
                        if s[r] {
                            sw += 1.0;
                            ss += r as f32;
                        }
                    }
                }
                let whole = if shift > HELL_ROWS / 2 {
                    shift as f32 - HELL_ROWS as f32
                } else {
                    shift as f32
                };
                out.push(whole + gs / gw - ss / sw);
            }
            k += per;
        }
        out
    }

    const MSG: &str = "CQ CQ DE W1AW W1AW TEST 1010 HELL ";

    /// What the decoder paints, against what was sent, as text art.
    /// `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn show() {
        let fs = 47_904.0;
        let sent = columns(&MSG.repeat(2));
        let x = hell_tx(&sent, fs, 1234.0, 300.0, 0.3);
        let (got, _) = decode(&x, fs);
        for r in 0..HELL_ROWS {
            let line: String = got
                .iter()
                .skip(100)
                .take(150)
                .map(|c| {
                    if c[r] > 191 {
                        '#'
                    } else if c[r] > 63 {
                        '+'
                    } else {
                        '.'
                    }
                })
                .collect();
            eprintln!("{line}");
        }
        eprintln!();
        for r in (0..HELL_ROWS).rev() {
            let line: String = sent
                .iter()
                .take(150)
                .map(|c| if c[r] { '#' } else { '.' })
                .collect();
            eprintln!("{line}");
        }
    }

    #[test]
    fn clean_columns_match_what_was_sent() {
        let fs = 47_904.0;
        let sent = top_first(&columns(&MSG.repeat(3)));
        let x = hell_tx(&columns(&MSG.repeat(3)), fs, 1234.0, 0.0, 0.3);
        let (got, st) = decode(&x, fs);
        assert!(
            st.center_hz.is_some_and(|c| (c - 1234.0).abs() < 3.0),
            "{st:?}"
        );
        // The received stream holds the sent columns in order, each moved
        // by the same number of rows (the column phase). Past the settling:
        let from = sent.len() / 3;
        let (_, shift, frac) = align(
            &got,
            &sent,
            from,
            sent.len(),
            -(from as i64)..=got.len() as i64,
        );
        assert!(frac > 0.98, "{frac}");
        // The ink is centered: the shift leaves the blank margins at the
        // column's ends, within a pixel.
        assert!(shift <= 1 || shift >= HELL_ROWS - 1, "{shift}");
    }

    /// 40 s of text with the transmitter's clock off: the text holds its
    /// row within a pixel from start to end, and the clock error is
    /// measured.
    #[test]
    fn columns_line_up_over_a_clock_error() {
        let fs = 47_904.0;
        for ppm in [100.0, -100.0, 1000.0, -1000.0] {
            let cols = columns(&MSG.repeat(3));
            let sent = top_first(&cols);
            let mut rng = Rng::new(4);
            let mut x = hell_tx(&cols, fs, 1500.0, ppm, 0.3);
            add_noise(&mut x, 0.3, 10.0, fs, &mut rng);
            let (got, st) = decode(&x, fs);
            // From 5 s in (the column phase is set after 3.4 s), in
            // stretches of 2 s.
            let off = offsets(&got, &sent, (5.0 * COLUMN_RATE) as usize, 35);
            assert!(off.len() >= 15, "{ppm} ppm: {off:?}");
            let lo = off.iter().cloned().fold(f32::INFINITY, f32::min);
            let hi = off.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
            assert!(hi - lo <= 1.0, "{ppm} ppm: rows {lo}..{hi}: {off:?}");
            let drift = st.drift_ppm.expect("drift measured");
            assert!(
                (drift as f64 - ppm).abs() < 20.0 + 0.1 * ppm.abs(),
                "{ppm}: measured {drift}"
            );
        }
    }

    /// The SNR reading (mean signal power against the noise in 2500 Hz)
    /// against what the test set: text keys the carrier on for the share
    /// of pixels that are ink.
    #[test]
    fn snr_reading() {
        let fs = 47_904.0;
        let sent = columns(&MSG.repeat(2));
        let duty = sent.iter().flat_map(|c| c.iter()).filter(|p| **p).count() as f32
            / (sent.len() * HELL_ROWS) as f32;
        for snr in [-3.0f32, 5.0, 15.0] {
            let mut rng = Rng::new(2);
            let mut x = hell_tx(&sent, fs, 1100.0, 0.0, 0.3);
            x.truncate(x.len() - fs as usize);
            add_noise(&mut x, 0.3, snr, fs, &mut rng);
            let (_, st) = decode(&x, fs);
            let want = snr + 10.0 * duty.log10();
            let got = st.snr_db.expect("snr");
            eprintln!("Hell at {want:.1} dB reads {got:.1}");
            assert!((got - want).abs() < 2.0, "{want}: {got}");
        }
    }

    #[test]
    fn copies_weak_and_fading() {
        let fs = 47_904.0;
        let sent = columns(&MSG.repeat(4));
        for (snr, fading) in [(-5.0, false), (0.0, true)] {
            let mut rng = Rng::new(9);
            let mut x = hell_tx(&sent, fs, 900.0, 50.0, 0.3);
            if fading {
                let g = rayleigh(x.len(), fs, 0.5, &mut rng);
                for (v, g) in x.iter_mut().zip(g) {
                    *v *= g.norm();
                }
            }
            add_noise(&mut x, 0.3, snr, fs, &mut rng);
            let (got, st) = decode(&x, fs);
            assert!(
                st.center_hz.is_some_and(|c| (c - 900.0).abs() < 15.0),
                "{snr}: {st:?}"
            );
            assert!(
                got.len() as f32 > 0.8 * sent.len() as f32,
                "{snr}: {} of {}",
                got.len(),
                sent.len()
            );
        }
    }

    /// Seconds of audio decoded per second of work. `--ignored
    /// --nocapture`.
    #[test]
    #[ignore]
    fn speed() {
        let fs = 47_904.0;
        let x = hell_tx(&columns(&MSG.repeat(3)), fs, 1500.0, 0.0, 0.3);
        let t = std::time::Instant::now();
        let _ = decode(&x, fs);
        eprintln!(
            "Feld Hell: {:.0}x real time",
            x.len() as f64 / fs as f64 / t.elapsed().as_secs_f64()
        );
    }

    /// The weakest signal the decoder locks on, and how many columns it
    /// then paints. `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn lock_sweep() {
        let fs = 47_904.0;
        let sent = columns(&MSG.repeat(2));
        for snr in [-3.0f32, -5.0, -6.0, -7.0, -8.0, -9.0, -10.0] {
            let mut rng = Rng::new(17);
            let mut x = hell_tx(&sent, fs, 1300.0, 50.0, 0.3);
            add_noise(&mut x, 0.3, snr, fs, &mut rng);
            let (got, st) = decode(&x, fs);
            eprintln!(
                "{snr} dB: locked {} at {:?}, {} of {} columns",
                st.locked,
                st.center_hz,
                got.len(),
                sent.len()
            );
        }
    }

    #[test]
    fn noise_gives_no_columns() {
        let fs = 47_904.0;
        let mut rng = Rng::new(1);
        let x: Vec<f32> = (0..(60.0 * fs) as usize)
            .map(|_| 0.3 * rng.gauss())
            .collect();
        let (got, st) = decode(&x, fs);
        assert!(got.is_empty() && !st.locked, "{} {st:?}", got.len());
    }
}
