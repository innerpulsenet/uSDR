//! MFSK16, MFSK32 and MFSK64 (Murray Greenman ZL1BPU and Nino Porcino
//! IZ8BLY), as fldigi sends them, with the inline pictures Shortwave
//! Radiogram sends in them.
//!
//! An MFSK signal is 16 tones one symbol rate apart, one at a time, phase
//! continuous: 15.625, 31.25 or 62.5 baud, 234, 469 or 938 Hz wide. Each
//! tone carries four bits (Gray coded), which come from the NASA K=7 rate
//! 1/2 convolutional code through a diagonal interleaver ([`fec`]); the
//! data bits are IZ8BLY Varicode ([`varicode`]).
//!
//! The audio is taken to 8 kHz, where a symbol is 512, 256 or 128 samples
//! and the tones fall on the bins of a DFT that long. An averaged spectrum
//! finds the signal: the comb of 16 tones whose bins hold the most power,
//! each of them clearly above the noise. The audio is mixed down by the
//! signal's center and each tone gets a sliding DFT a symbol long, so the
//! 16 tone powers are known at every sample. The symbol clock is the
//! instant within a symbol where one tone stands out most clearly,
//! averaged over the last few dozen symbols and followed by a
//! second-order loop that also learns the transmitter's clock against
//! this receiver's. The tone powers at each symbol instant become a
//! log-likelihood for each of the four bits (against a noise floor taken
//! from the 15 tones that were not sent), which are deinterleaved and
//! decoded by a Viterbi decoder. The frequency is followed from the phase
//! turn of the sent tone between the two halves of each symbol.
//!
//! A squelch keeps noise from printing: every decoded bit is encoded again
//! and compared with what was received; a real signal's errors are what
//! the code can correct, noise disagrees on nearly half.
//!
//! Pictures: after the text `Pic:WxH;` (gray) or `Pic:WxHC;` (color,
//! optionally `p4` or `p2` before the `;` for 4 or 2 samples a pixel
//! instead of 8), the transmitter flushes the code with 107 zero bits
//! (180 for MFSK64), sends 352 samples of the lowest tone, then the pixels
//! as an FM tone, 8 samples (1 ms) each, black at the lowest tone and
//! white at the highest; a color picture sends each row three times, red,
//! green, blue. The decoder works out from the bit that ended the header
//! exactly which symbol the flush ends on, and so the sample where the
//! first pixel begins; pixels are the mean instantaneous frequency over
//! their span, scaled by the transmitter's clock as the symbol loop has
//! measured it.
//!
//! Measured on the test channel (40 ppm of clock error), the SNR in
//! 2500 Hz where fewer than 2% of characters are wrong: MFSK16 about
//! -12 dB, MFSK32 -10 dB, MFSK64 -8 dB; in flat Rayleigh fading (1 Hz)
//! MFSK16 -8 dB and MFSK32 -2 dB, while MFSK64, whose interleaver spans
//! only 0.64 s, loses a quarter of its characters even at 0 dB. A color
//! picture at 15 dB comes out within about 10 levels of 256 on average.
//!
//! Not decoded: the other MFSK speeds (MFSK4, 8, 11, 22, 31, 128 and the
//! long-interleave variants), signals with the tones reversed (lower
//! sideband), and pictures in those modes.

pub(crate) mod fec;
pub(crate) mod resample;
pub(crate) mod varicode;

use crate::rtty::Lines;
use fec::{Encoder, Interleaver, Viterbi};
use num_complex::Complex32;
use resample::Resampler;
use rustfft::{Fft, FftPlanner};
use std::collections::VecDeque;
use std::f64::consts::TAU;
use std::sync::Arc;

/// The rate MFSK is defined at.
const RATE: f64 = 8000.0;
const TONES: usize = 16;
const BITS: usize = 4;

/// Data bits the Viterbi decoder waits before deciding one. The picture
/// timing needs the header's end decided before the flush after it is
/// over: a delay under 46 does that in every mode.
const VITERBI_DELAY: u32 = 40;
/// Log-likelihood ratios are limited to this.
const LLR_MAX: f32 = 16.0;
/// The noise floor (per tone bin) follows over about this many symbols.
const NOISE_TAU: f32 = 16.0;

/// The timing profile forgets over about this many symbols.
const PROFILE_TAU: f32 = 24.0;
/// The timing profile's power reference follows over this many symbols.
const LEVEL_SYMS: f32 = 4.0;
/// While nothing decodes, the symbol clock moves to the profile's highest
/// point when it stands this much above the point the clock is on.
const JUMP_RATIO: f32 = 1.1;
/// Symbols of profile before the symbol clock starts.
const PROFILE_FIRST: u64 = 16;
/// The symbol clock loop's gains on the timing error (in samples): phase
/// and rate.
const TIMING_GAIN: f64 = 0.05;
const TIMING_RATE_GAIN: f64 = 0.0004;
/// The frequency loop's step per symbol, as a share of the measured error.
const AFC_GAIN: f64 = 0.05;
/// The transmitter's symbol period is the slope of a straight line through
/// the symbol instants while the squelch is open, used once it spans this
/// many symbols (and reported once it spans twice as many). The loop's
/// instants scatter by a sample or so; over 64 symbols the slope is good
/// to about a hundred parts per million, over 128 to about 40.
const CLOCK_FIT_SYMS: f64 = 64.0;

/// The squelch: the share of received coded bits that disagree with the
/// decoded bits encoded again, over the last [`BER_WINDOW`] coded bits.
/// On noise the decoder still finds a path that agrees with most of what
/// came in: the share is 0.29 on average and spreads by about 0.035 over
/// this window. A signal the code copies cleanly stays under 0.1.
const BER_WINDOW: usize = 192;
const BER_OPEN: f32 = 0.11;
const BER_CLOSE: f32 = 0.2;
/// A character prints once this many data bits after it have checked out
/// (their coded bits disagreeing no more than [`BER_CLOSE`]).
const VOUCH_BITS: u64 = 24;
/// Characters ended this many data bits before the squelch opens print
/// when it does.
const HELD_BITS: u64 = BER_WINDOW as u64 / 2 + 24;

/// The search: spectrum averaging time, how often it looks, how long the
/// squelch must have been shut for it to move the receiver.
const SEARCH_TAU_S: f64 = 4.0;
const SEARCH_EVERY_S: f64 = 1.0;
const SEARCH_MOVE_AFTER_S: f64 = 8.0;
/// The search waits for this much audio before its first answer.
const SEARCH_MIN_S: f64 = 2.5;
/// A receiver that has not copied anything where it is tuned moves to a
/// better comb after this long.
const SEARCH_FIRST_MOVE_S: f64 = 3.0;
/// Audio kept, and run through the receiver again when it retunes.
const REPLAY_S: f64 = 10.0;
/// Where the signal may be: its lowest tone at least this, its highest at
/// most that.
const LO_HZ: f64 = 300.0;
const HI_HZ: f64 = 2700.0;
/// A comb is a signal when its tones average this much power above the
/// noise (a ratio) and most of them each show at least this share of that.
const COMB_EXCESS: f32 = 0.25;
const COMB_EVEN: f32 = 0.35;
/// With a center given, the search reaches this far from it.
const SEARCH_NEAR_HZ: f64 = 100.0;

/// Pending text is let go after this long without a character.
const FLUSH_S: f64 = 2.0;
/// The picture's lead-in of the lowest tone, in samples at 8 kHz.
const PROLOGUE: f64 = 352.0;
/// The largest picture accepted (Radiogram sends up to about 320 wide).
const PIC_MAX_SIDE: u32 = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MfskMode {
    Mfsk16,
    Mfsk32,
    Mfsk64,
}

impl MfskMode {
    /// Samples a symbol at 8 kHz.
    pub fn symbol_len(&self) -> usize {
        match self {
            MfskMode::Mfsk16 => 512,
            MfskMode::Mfsk32 => 256,
            MfskMode::Mfsk64 => 128,
        }
    }

    pub fn baud(&self) -> f32 {
        (RATE / self.symbol_len() as f64) as f32
    }

    /// From the lowest tone to the highest.
    pub fn bandwidth_hz(&self) -> f32 {
        15.0 * self.baud()
    }

    pub fn name(&self) -> &'static str {
        match self {
            MfskMode::Mfsk16 => "MFSK16",
            MfskMode::Mfsk32 => "MFSK32",
            MfskMode::Mfsk64 => "MFSK64",
        }
    }

    fn depth(&self) -> usize {
        10
    }

    /// Zero bits the transmitter flushes the code with.
    fn flush_bits(&self) -> u64 {
        match self {
            MfskMode::Mfsk64 => 180,
            _ => 107,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MfskConfig {
    pub mode: MfskMode,
    /// The signal's center (midway between the lowest and highest tones,
    /// fldigi's dial reading). None searches 300 to 2700 Hz.
    pub center_hz: Option<f32>,
}

impl Default for MfskConfig {
    fn default() -> Self {
        Self {
            mode: MfskMode::Mfsk32,
            center_hz: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum MfskEvent {
    /// Decoded text, a line at a time (or what there is after a pause).
    Text {
        text: String,
        mode: MfskMode,
        center_hz: f32,
        snr_db: f32,
    },
    /// A picture header was decoded; the picture follows.
    PictureStart {
        width: u32,
        height: u32,
        color: bool,
    },
    /// A received picture: `width` by `height` gray bytes, or three bytes
    /// (red, green, blue) a pixel for color, pixel after pixel, row after
    /// row from the top.
    /// `complete` is false when it was cut short (the decoder was reset or
    /// retuned before its end); missing pixels are 0.
    Picture {
        width: u32,
        height: u32,
        color: bool,
        pixels: Vec<u8>,
        complete: bool,
    },
}

impl MfskEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            MfskEvent::Text { .. } => "text",
            MfskEvent::PictureStart { .. } => "pictureStart",
            MfskEvent::Picture { .. } => "picture",
        }
    }

    pub fn summary(&self) -> String {
        match self {
            MfskEvent::Text { text, mode, .. } => format!("{}: {}", mode.name(), text),
            MfskEvent::PictureStart {
                width,
                height,
                color,
            } => {
                format!(
                    "picture {}x{} {} coming",
                    width,
                    height,
                    if *color { "color" } else { "gray" }
                )
            }
            MfskEvent::Picture {
                width,
                height,
                color,
                complete,
                ..
            } => format!(
                "picture {}x{} {}{}",
                width,
                height,
                if *color { "color" } else { "gray" },
                if *complete { "" } else { " (cut short)" }
            ),
        }
    }

    /// Fields with camelCase keys. A picture's pixels are given in base64.
    pub fn fields(&self) -> Vec<(&'static str, String)> {
        match self {
            MfskEvent::Text {
                text,
                mode,
                center_hz,
                snr_db,
            } => vec![
                ("text", text.clone()),
                ("mode", mode.name().into()),
                ("centerHz", format!("{center_hz:.1}")),
                ("snrDb", format!("{snr_db:.1}")),
            ],
            MfskEvent::PictureStart {
                width,
                height,
                color,
            } => {
                vec![
                    ("width", width.to_string()),
                    ("height", height.to_string()),
                    ("color", color.to_string()),
                ]
            }
            MfskEvent::Picture {
                width,
                height,
                color,
                pixels,
                complete,
            } => vec![
                ("width", width.to_string()),
                ("height", height.to_string()),
                ("color", color.to_string()),
                ("complete", complete.to_string()),
                ("pixelsB64", base64(pixels)),
            ],
        }
    }
}

fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for (i, shift) in [18, 12, 6, 0].iter().enumerate() {
            if i <= c.len() {
                s.push(A[(n >> shift) as usize & 63] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

#[derive(Clone, Debug, PartialEq)]
pub struct MfskStatus {
    /// The squelch is open: the signal decodes cleanly.
    pub locked: bool,
    pub mode: MfskMode,
    /// Where the receiver is tuned (the signal's center), once it has
    /// found or been given one.
    pub center_hz: Option<f32>,
    /// Its lowest tone.
    pub lower_tone_hz: Option<f32>,
    /// Signal to noise in 2500 Hz, from the sent tones' power against the
    /// others', while locked.
    pub snr_db: Option<f32>,
    /// Coded bits that disagreed with the decoder's choice, recently (0..1).
    pub bit_error_rate: f32,
    /// The transmitter's clock against this receiver's, parts per million,
    /// as the symbol clock has learned it.
    pub clock_ppm: Option<f32>,
    /// A picture being received: width, height, color, and the share done.
    pub picture: Option<(u32, u32, bool, f32)>,
    /// Characters printed since the decoder was made.
    pub chars: u64,
}

// ---------------------------------------------------------------------------
// The search.

struct Search {
    fft: Arc<dyn Fft<f32>>,
    size: usize,
    win: Vec<f32>,
    buf: Vec<Complex32>,
    scratch: Vec<Complex32>,
    ring: Vec<f32>,
    pos: usize,
    since: usize,
    hop: usize,
    avg: Vec<f32>,
    frames: u64,
    alpha: f32,
}

impl Search {
    /// Bins a quarter of the tone spacing apart; a Hann window two symbols
    /// long.
    fn new(l: usize) -> Self {
        let size = 4 * l;
        let wlen = 2 * l;
        let fft = FftPlanner::new().plan_fft_forward(size);
        let scratch = vec![Complex32::new(0.0, 0.0); fft.get_inplace_scratch_len()];
        let win = (0..wlen)
            .map(|i| (0.5 - 0.5 * (TAU * i as f64 / wlen as f64).cos()) as f32)
            .collect();
        let hop = l / 2;
        Self {
            fft,
            size,
            win,
            buf: vec![Complex32::new(0.0, 0.0); size],
            scratch,
            ring: vec![0.0; wlen],
            pos: 0,
            since: 0,
            hop,
            avg: vec![0.0; size / 2],
            frames: 0,
            alpha: (hop as f64 / (SEARCH_TAU_S * RATE)) as f32,
        }
    }

    fn push(&mut self, x: f32) -> bool {
        let wlen = self.win.len();
        self.ring[self.pos] = x;
        self.pos = (self.pos + 1) % wlen;
        self.since += 1;
        if self.since < self.hop {
            return false;
        }
        self.since = 0;
        for i in 0..wlen {
            self.buf[i] = Complex32::new(self.ring[(self.pos + i) % wlen] * self.win[i], 0.0);
        }
        for z in self.buf[wlen..].iter_mut() {
            *z = Complex32::new(0.0, 0.0);
        }
        self.fft
            .process_with_scratch(&mut self.buf, &mut self.scratch);
        let a = self.alpha.max(1.0 / (self.frames + 1) as f32);
        for (v, z) in self.avg.iter_mut().zip(&self.buf) {
            *v += a * (z.norm_sqr() - *v);
        }
        self.frames += 1;
        true
    }

    /// The best comb of 16 tones whose lowest tone lies in `lo..hi` Hz:
    /// that tone's frequency and how far the comb stands above the noise.
    fn best(&self, lo: f64, hi: f64) -> Option<(f64, f32)> {
        let bin_hz = RATE / self.size as f64;
        let b_lo = ((LO_HZ / bin_hz).ceil() as usize).max(1);
        let b_hi = ((HI_HZ / bin_hz).floor() as usize).min(self.avg.len() - 2);
        let mut band: Vec<f32> = self.avg[b_lo..=b_hi].to_vec();
        // A signal covers 64 of these bins: up to two fifths of the band for
        // MFSK64. The 30th percentile is still noise.
        let at = band.len() * 3 / 10;
        band.select_nth_unstable_by(at, |a, b| a.total_cmp(b));
        // Never below 40 dB under the strongest bin: a silent input's floor
        // is the window's far sidelobes, which are no signal.
        let top = self.avg[b_lo..=b_hi].iter().cloned().fold(0.0f32, f32::max);
        let floor = band[at].max(1e-4 * top);
        if floor <= 0.0 {
            return None;
        }
        let first = ((lo / bin_hz).ceil() as usize).max(b_lo);
        let last = ((hi / bin_hz).floor() as usize).min(b_hi.saturating_sub(4 * (TONES - 1)));
        let mut best: Option<(usize, f32)> = None;
        let mut ex = [0f32; TONES];
        for b in first..=last {
            for (k, e) in ex.iter_mut().enumerate() {
                *e = self.avg[b + 4 * k] / floor - 1.0;
            }
            let mean = ex.iter().sum::<f32>() / TONES as f32;
            if mean < COMB_EXCESS {
                continue;
            }
            let even = ex.iter().filter(|e| **e >= COMB_EVEN * mean).count();
            if even < 12 {
                continue;
            }
            if best.is_none_or(|(_, m)| mean > m) {
                best = Some((b, mean));
            }
        }
        // Between bins: a parabola through the comb's mean power at the
        // bins either side (each a quarter tone apart).
        best.map(|(b, m)| {
            let comb = |b: usize| (0..TONES).map(|k| self.avg[b + 4 * k]).sum::<f32>();
            let frac = if b > 0 && b + 1 + 4 * (TONES - 1) < self.avg.len() {
                let (l, c, r) = (comb(b - 1), comb(b), comb(b + 1));
                let den = l - 2.0 * c + r;
                if den < 0.0 {
                    (0.5 * (l - r) / den).clamp(-0.5, 0.5) as f64
                } else {
                    0.0
                }
            } else {
                0.0
            };
            ((b as f64 + frac) * bin_hz, m)
        })
    }
}

// ---------------------------------------------------------------------------
// The picture.

struct Picture {
    width: u32,
    height: u32,
    color: bool,
    /// First pixel's start, in samples at 8 kHz on the input's clock, and
    /// a pixel's length.
    start: f64,
    spp: f64,
    /// Instantaneous frequency summed over the current pixel (radians)
    /// and the time it covers.
    phase: f64,
    span: f64,
    index: usize,
    pixels: Vec<u8>,
    total: usize,
}

impl Picture {
    fn value(&self, bw: f64) -> u8 {
        let hz = if self.span > 0.0 {
            self.phase / self.span * RATE / TAU
        } else {
            -bw / 2.0
        };
        (256.0 * (hz + bw / 2.0) / bw).clamp(0.0, 255.0) as u8
    }

    /// Where pass `i` (row, then color within it) puts its pixels.
    fn store(&mut self, v: u8) {
        let w = self.width as usize;
        let i = self.index;
        if self.color {
            let row = i / (3 * w);
            let c = (i / w) % 3;
            let col = i % w;
            self.pixels[(row * w + col) * 3 + c] = v;
        } else {
            self.pixels[i] = v;
        }
        self.index += 1;
    }
}

/// Parse `Pic:WxH;`, `Pic:WxHC;`, with an optional `p4` or `p2` before
/// the `;`, at the end of `s`: width, height, color, samples a pixel.
fn parse_header(s: &str) -> Option<(u32, u32, bool, u32)> {
    let at = s.rfind("Pic:")?;
    let rest = s[at + 4..].strip_suffix(';')?;
    let (dims, spp) = match rest.find('p') {
        Some(p) => (
            &rest[..p],
            match &rest[p..] {
                "p4" => 4,
                "p2" => 2,
                "p8" => 8,
                _ => return None,
            },
        ),
        None => (rest, 8),
    };
    let (dims, color) = match dims.strip_suffix('C') {
        Some(d) => (d, true),
        None => (dims, false),
    };
    let (w, h) = dims.split_once('x')?;
    let (w, h): (u32, u32) = (w.parse().ok()?, h.parse().ok()?);
    (w > 0 && h > 0 && w <= PIC_MAX_SIDE && h <= PIC_MAX_SIDE).then_some((w, h, color, spp))
}

// ---------------------------------------------------------------------------
// The decoder.

pub struct MfskDecoder {
    cfg: MfskConfig,
    resampler: Resampler,
    tmp: Vec<f32>,
    /// Static crashes zeroed before anything else sees them (the RTTY
    /// decoder's blanker, told where the tones are once the copy is in).
    blanker: crate::rtty::Blanker,
    /// Samples at 8 kHz taken in.
    n: u64,
    l: usize,
    search: Search,
    search_every: u64,
    /// The signal's center as tuned, Hz.
    center: Option<f64>,
    osc: f64,
    /// Per tone: the product ring and the full- and half-symbol sums.
    rot: Vec<Complex32>,
    prod: Vec<Complex32>,
    full: [Complex32; TONES],
    half: [Complex32; TONES],
    /// The timing profile, one bin a sample of the symbol.
    profile: Vec<f32>,
    profiled: u64,
    /// The symbol the clock last jumped at.
    last_jump: u64,
    /// The band's power averaged over a few symbols.
    level: f32,
    /// When the next symbol ends (samples), the symbol period, and the
    /// symbols taken.
    next_sym: Option<f64>,
    period: f64,
    syms: u64,
    /// When each recent symbol ended.
    sym_end: VecDeque<(u64, f64)>,
    /// No symbols from this one on (a picture follows).
    sym_stop: Option<u64>,
    /// Sums for the straight line through the symbol instants while the
    /// squelch is open: n, sum k, sum t, sum k^2, sum k t (k counted from
    /// `fit_k0`, t from `fit_t0`).
    fit: [f64; 5],
    fit_k0: u64,
    fit_t0: f64,
    noise: f32,
    snr_acc: f32,
    deint: Interleaver<f32>,
    viterbi: Viterbi,
    encoder: Encoder,
    /// Received hard coded bits by data bit index.
    hard: VecDeque<(u64, bool, bool, bool)>,
    errors: VecDeque<u8>,
    err_sum: u32,
    open: bool,
    shreg: u32,
    lines: Lines,
    last_char: f64,
    recent: String,
    chars: u64,
    shut_since: f64,
    tuned_at: f64,
    opened_here: bool,
    /// Characters decoded while the squelch was shut, by the data bit that
    /// ended them, in case the window that opens it vouches for them.
    held: VecDeque<(u64, char)>,
    /// Characters waiting for the bits after them to check out.
    vouch: VecDeque<(u64, char)>,
    hist: VecDeque<f32>,
    /// Replaying the history to learn the timing profile only.
    training: bool,
    // Pictures.
    wide: Option<WideFilter>,
    picture: Option<Picture>,
    events: Vec<MfskEvent>,
}

/// A complex lowpass for the picture's FM, the same delay every time.
struct WideFilter {
    taps: Vec<f32>,
    hist: Vec<Complex32>,
    pos: usize,
    prev: Complex32,
}

const WIDE_TAPS: usize = 111;

impl WideFilter {
    fn new(cut_hz: f64) -> Self {
        let n = WIDE_TAPS;
        let fc = cut_hz / RATE;
        let mut taps: Vec<f32> = (0..n)
            .map(|i| {
                let x = i as f64 - (n - 1) as f64 / 2.0;
                let s = if x == 0.0 {
                    2.0 * fc
                } else {
                    (TAU * fc * x).sin() / (std::f64::consts::PI * x)
                };
                let u = i as f64 / (n - 1) as f64;
                (s * (0.42 - 0.5 * (TAU * u).cos() + 0.08 * (2.0 * TAU * u).cos())) as f32
            })
            .collect();
        let sum: f32 = taps.iter().sum();
        taps.iter_mut().for_each(|t| *t /= sum);
        Self {
            taps,
            hist: vec![Complex32::new(0.0, 0.0); n],
            pos: 0,
            prev: Complex32::new(0.0, 0.0),
        }
    }

    /// The filter's delay in samples.
    fn delay() -> f64 {
        (WIDE_TAPS - 1) as f64 / 2.0
    }

    /// One sample in; the phase turn since the last output, radians.
    fn push(&mut self, z: Complex32) -> f64 {
        let n = self.hist.len();
        self.hist[self.pos] = z;
        self.pos = (self.pos + 1) % n;
        let mut acc = Complex32::new(0.0, 0.0);
        for (i, t) in self.taps.iter().enumerate() {
            acc += self.hist[(self.pos + i) % n] * *t;
        }
        let d = (acc * self.prev.conj()).arg() as f64;
        self.prev = acc;
        d
    }
}

impl MfskDecoder {
    pub fn new(fs: f32, cfg: MfskConfig) -> Self {
        let l = cfg.mode.symbol_len();
        // The rotators: tone k is (k - 7.5) bins from the center, so its
        // phase repeats every two symbols.
        let mut rot = Vec::with_capacity(TONES * 2 * l);
        for k in 0..TONES {
            for m in 0..2 * l {
                let ph = -TAU * (k as f64 - 7.5) * m as f64 / l as f64;
                rot.push(Complex32::new(ph.cos() as f32, ph.sin() as f32));
            }
        }
        let search = Search::new(l);
        let search_every = (SEARCH_EVERY_S * RATE / search.hop as f64).round() as u64;
        let mut d = Self {
            cfg,
            resampler: Resampler::new(fs, RATE as f32, 3000.0, 5000.0),
            tmp: Vec::with_capacity(4096),
            blanker: crate::rtty::Blanker::new(RATE as f32),
            n: 0,
            l,
            search,
            search_every,
            center: None,
            osc: 0.0,
            rot,
            prod: vec![Complex32::new(0.0, 0.0); TONES * l],
            full: [Complex32::new(0.0, 0.0); TONES],
            half: [Complex32::new(0.0, 0.0); TONES],
            profile: vec![0.0; l],
            profiled: 0,
            last_jump: 0,
            level: 0.0,
            next_sym: None,
            period: l as f64,
            syms: 0,
            sym_end: VecDeque::new(),
            sym_stop: None,
            fit: [0.0; 5],
            fit_k0: 0,
            fit_t0: 0.0,
            noise: 0.0,
            snr_acc: 0.0,
            deint: Interleaver::new(BITS, cfg.mode.depth(), false, 0.0),
            viterbi: Viterbi::new(VITERBI_DELAY),
            encoder: Encoder::default(),
            hard: VecDeque::new(),
            errors: VecDeque::new(),
            err_sum: 0,
            open: false,
            shreg: 1,
            lines: Lines::default(),
            last_char: 0.0,
            recent: String::new(),
            chars: 0,
            shut_since: 0.0,
            tuned_at: 0.0,
            opened_here: false,
            held: VecDeque::new(),
            vouch: VecDeque::new(),
            hist: VecDeque::new(),
            training: false,
            wide: None,
            picture: None,
            events: Vec::new(),
        };
        if let Some(c) = cfg.center_hz {
            d.tune(c as f64);
        }
        d
    }

    pub fn push(&mut self, audio: &[f32]) {
        self.tmp.clear();
        self.resampler.process(audio, &mut self.tmp);
        let tmp = std::mem::take(&mut self.tmp);
        for &x in &tmp {
            if let Some((y, _)) = self.blanker.push(x) {
                self.sample(y);
            }
        }
        self.tmp = tmp;
        let half = self.cfg.mode.bandwidth_hz() as f64 / 2.0;
        self.blanker.set_pair(self.center.filter(|_| self.open).map(|c| ((c - half) as f32, (c + half) as f32)));
        if !self.lines.is_empty() && self.now_s() - self.last_char > FLUSH_S {
            self.flush_text();
        }
    }

    pub fn take(&mut self) -> Vec<MfskEvent> {
        std::mem::take(&mut self.events)
    }

    /// Print whatever text is pending.
    pub fn flush(&mut self) {
        self.flush_text();
    }

    pub fn status(&self) -> MfskStatus {
        let bw = self.cfg.mode.bandwidth_hz() as f64;
        MfskStatus {
            locked: self.open,
            mode: self.cfg.mode,
            center_hz: self.center.map(|c| c as f32),
            lower_tone_hz: self.center.map(|c| (c - bw / 2.0) as f32),
            snr_db: self.open.then(|| self.snr_db()),
            bit_error_rate: self.ber(),
            clock_ppm: (self.fit[0] >= 2.0 * CLOCK_FIT_SYMS)
                .then(|| ((self.measured_period() / self.l as f64 - 1.0) * 1e6) as f32),
            picture: self
                .picture
                .as_ref()
                .map(|p| (p.width, p.height, p.color, p.index as f32 / p.total as f32)),
            chars: self.chars,
        }
    }

    fn now_s(&self) -> f64 {
        self.n as f64 / RATE
    }

    fn ber(&self) -> f32 {
        if self.errors.is_empty() {
            0.5
        } else {
            self.err_sum as f32 / self.errors.len() as f32
        }
    }

    fn snr_db(&self) -> f32 {
        // A sine of amplitude A puts (A L / 2)^2 in its tone's bin; noise of
        // variance v puts v L in every bin: SNR in 2500 Hz at 8 kHz is
        // 3.2 S / (L N).
        10.0 * (3.2 * self.snr_acc.max(1e-6) / self.l as f32).log10()
    }

    /// Tune to a center and start the symbol clock over, on the last
    /// [`REPLAY_S`] of audio: a signal is found a few seconds after it
    /// starts, and its first words are in those seconds.
    fn tune(&mut self, center: f64) {
        self.center = Some(center);
        self.osc = 0.0;
        self.restart();
        self.tuned_at = self.now_s();
        self.opened_here = false;
        let hist = std::mem::take(&mut self.hist);
        let first = self.n - hist.len() as u64;
        // First the timing profile alone, over all of it, so the symbol
        // clock is right from the first symbol of the second pass.
        self.training = true;
        for (i, &x) in hist.iter().enumerate() {
            self.demod(x, first + i as u64);
        }
        self.training = false;
        let profile = std::mem::take(&mut self.profile);
        let profiled = self.profiled;
        self.restart();
        self.profile = profile;
        self.profiled = profiled;
        self.osc = 0.0;
        for (i, &x) in hist.iter().enumerate() {
            self.demod(x, first + i as u64);
        }
        self.hist = hist;
    }

    fn restart(&mut self) {
        if self.picture.is_some() {
            self.finish_picture(false);
        }
        self.prod.fill(Complex32::new(0.0, 0.0));
        self.full = [Complex32::new(0.0, 0.0); TONES];
        self.half = [Complex32::new(0.0, 0.0); TONES];
        self.profile.fill(0.0);
        self.profiled = 0;
        self.next_sym = None;
        self.period = self.l as f64;
        self.fit = [0.0; 5];
        self.syms = 0;
        self.last_jump = 0;
        self.sym_end.clear();
        self.noise = 0.0;
        self.deint = Interleaver::new(BITS, self.cfg.mode.depth(), false, 0.0);
        self.viterbi.reset();
        self.encoder = Encoder::default();
        self.hard.clear();
        self.errors.clear();
        self.err_sum = 0;
        self.set_open(false);
        self.shreg = 1;
    }

    fn sample(&mut self, x: f32) {
        let n = self.n;
        self.n += 1;
        self.hist.push_back(x);
        if self.hist.len() > (REPLAY_S * RATE) as usize {
            self.hist.pop_front();
        }
        // A retune runs the receiver over the history, this sample included.
        if self.search.push(x)
            && self.search.frames.is_multiple_of(self.search_every)
            && self.look()
        {
            return;
        }
        self.demod(x, n);
    }

    /// The receiver proper, sample `n`.
    fn demod(&mut self, x: f32, n: u64) {
        let Some(center) = self.center else { return };
        // Mix the center to zero.
        let z = Complex32::new(
            (x as f64 * self.osc.cos()) as f32,
            (-x as f64 * self.osc.sin()) as f32,
        );
        self.osc = (self.osc + TAU * center / RATE) % TAU;

        let in_picture = self.picture_sample(z, n);

        let l = self.l;
        let m = (n % (2 * l as u64)) as usize;
        let slot = (n % l as u64) as usize;
        let half_slot = ((n + l as u64 / 2) % l as u64) as usize;
        let mut total = 0.0f32;
        let mut top = 0.0f32;
        for k in 0..TONES {
            let p = z * self.rot[k * 2 * l + m];
            let ring = &mut self.prod[k * l..(k + 1) * l];
            self.full[k] += p - ring[slot];
            self.half[k] += p - ring[half_slot];
            ring[slot] = p;
            let e = self.full[k].norm_sqr();
            total += e;
            top = top.max(e);
        }
        // Float sums drift; start them afresh now and then.
        if n.is_multiple_of(8192) {
            self.resum(slot);
        }
        // The sums go on through a picture, so they are full when the
        // symbols after it begin; nothing else does.
        if in_picture {
            return;
        }
        let a = 1.0 / PROFILE_TAU.min(self.profiled as f32 / l as f32 + 1.0);
        // The timing profile: the strongest tone's power, by where in the
        // symbol this sample falls, against the band's power over the last
        // few symbols. A window d samples off a symbol that changes tone
        // holds (1 - d/L)^2 of it, which peaks sharply where the window
        // fits; the strongest tone's share of the window's own power would
        // stay near 1 for any small d on a clean signal.
        self.level += (total - self.level) / (LEVEL_SYMS * l as f32);
        if self.level > 0.0 {
            self.profile[slot] += a * (top / self.level - self.profile[slot]);
        }
        self.profiled += 1;
        if self.training {
            return;
        }

        match self.next_sym {
            None => {
                if self.profiled >= PROFILE_FIRST * l as u64 {
                    // Start the clock at the profile's peak.
                    let (at, _) =
                        self.profile
                            .iter()
                            .enumerate()
                            .fold(
                                (0, f32::MIN),
                                |b, (i, v)| if *v > b.1 { (i, *v) } else { b },
                            );
                    let ahead = (at + l - slot) % l;
                    self.next_sym = Some(n as f64 + ahead.max(1) as f64);
                }
            }
            Some(t) => {
                // Before a picture, the last symbol is the flush's.
                let stopped = self.sym_stop.is_some_and(|s| self.syms >= s);
                if n as f64 + 0.5 >= t && !stopped {
                    self.symbol(t);
                }
            }
        }
    }

    fn resum(&mut self, slot: usize) {
        let l = self.l;
        for k in 0..TONES {
            let ring = &self.prod[k * l..(k + 1) * l];
            self.full[k] = ring.iter().sum();
            // The half window is the last l/2 products, ending at `slot`.
            let mut h = Complex32::new(0.0, 0.0);
            for i in 0..l / 2 {
                h += ring[(slot + l - i) % l];
            }
            self.half[k] = h;
        }
    }

    /// A symbol ends now (due at `t`).
    fn symbol(&mut self, t: f64) {
        let l = self.l as f64;
        let e: Vec<f32> = self.full.iter().map(|z| z.norm_sqr()).collect();
        let (best, emax) = e
            .iter()
            .enumerate()
            .fold((0, 0.0f32), |b, (i, v)| if *v > b.1 { (i, *v) } else { b });
        let sum: f32 = e.iter().sum();
        let rest = (sum - emax) / (TONES - 1) as f32;
        if self.noise <= 0.0 {
            self.noise = rest.max(1e-30);
        } else {
            self.noise += (rest - self.noise) / NOISE_TAU;
        }
        let n0 = self.noise.max(1e-30);

        // Timing: the profile's peak near where this symbol was due (once
        // the profile has something to say).
        let due = t.rem_euclid(l);
        let mut err = if self.profiled >= PROFILE_FIRST * self.l as u64 {
            self.timing_error(due)
        } else {
            0.0
        };
        let mut jump = false;
        // While nothing decodes, the clock may be on a wrong peak (it
        // started on too little profile, or the signal changed): if the
        // profile's highest point is clearly elsewhere, go there. Both are
        // judged on the profile smoothed over a sixteenth of a symbol, and
        // a jump waits for the profile to have learned from the symbols
        // since the last, so a weak signal's ragged profile does not throw
        // the clock about.
        if !self.open
            && self.profiled >= PROFILE_FIRST * self.l as u64
            && self.syms >= self.last_jump + PROFILE_TAU as u64
        {
            let w = (self.l / 32) as i64;
            let smooth = |i: i64| -> f32 {
                (i - w..=i + w)
                    .map(|j| self.profile[j.rem_euclid(self.l as i64) as usize])
                    .sum::<f32>()
                    / (2 * w + 1) as f32
            };
            let (g, gv) = (0..self.l as i64)
                .map(|i| (i, smooth(i)))
                .fold((0, f32::MIN), |b, x| if x.1 > b.1 { x } else { b });
            let local = smooth((due + err).round() as i64);
            if gv > JUMP_RATIO * local {
                let mut d = g as f64 - due;
                d -= l * (d / l).round();
                err = d;
                jump = true;
                self.last_jump = self.syms;
            }
        }
        // The rate learns only from a signal that decodes: on noise the
        // profile's peak wanders.
        // Nor from the pull-in after a jump or a retune, which would wind
        // it up far past the truth.
        if self.open && !jump && err.abs() < l / 64.0 {
            self.period += TIMING_RATE_GAIN * err;
            self.period = self.period.clamp(l * 0.998, l * 1.002);
        }
        // The line through the instants: started afresh at a jump.
        if jump || !self.open {
            self.fit = [0.0; 5];
        } else {
            if self.fit[0] == 0.0 {
                self.fit_k0 = self.syms;
                self.fit_t0 = t;
            }
            let k = (self.syms - self.fit_k0) as f64;
            let y = t - self.fit_t0;
            let f = &mut self.fit;
            f[0] += 1.0;
            f[1] += k;
            f[2] += y;
            f[3] += k * k;
            f[4] += k * y;
        }
        self.next_sym = Some(t + self.period + if jump { err } else { TIMING_GAIN * err });
        let r = self.syms;
        self.syms += 1;
        self.sym_end.push_back((r, t));
        while self.sym_end.len() > 64 {
            self.sym_end.pop_front();
        }

        // Frequency: the sent tone's phase turn from the first half of the
        // symbol to the second.
        // While nothing decodes, only from symbols that stand well clear of
        // the noise, and more gently: the search puts the receiver within
        // a quarter tone or so, and this pulls it the rest of the way.
        let afc = if self.open {
            (emax > 4.0 * n0).then_some(AFC_GAIN)
        } else {
            (emax > 8.0 * n0).then_some(AFC_GAIN / 2.0)
        };
        if let Some(gain) = afc {
            let h2 = self.half[best];
            let h1 = self.full[best] - h2;
            let dphi = (h2 * h1.conj()).arg() as f64;
            let err_hz = dphi / TAU * RATE / (l / 2.0);
            let reach = self.cfg.mode.baud() as f64 / 2.0;
            if let Some(c) = self.center.as_mut() {
                *c += gain * err_hz.clamp(-reach, reach);
            }
        }
        if self.open {
            self.snr_acc += ((emax - n0) / n0 - self.snr_acc) / 32.0;
        }

        // Bits, first (highest) first: the log-likelihood that each is 0.
        let mut soft = [0f32; BITS];
        for (b, s) in soft.iter_mut().enumerate() {
            let shift = BITS - 1 - b;
            let (mut m0, mut m1) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
            let mut terms = [(0f32, false); TONES];
            for (k, ek) in e.iter().enumerate() {
                let v = ek / n0;
                let one = (ungray(k) >> shift) & 1 == 1;
                terms[k] = (v, one);
                if one { m1 = m1.max(v) } else { m0 = m0.max(v) }
            }
            let (mut s0, mut s1) = (0.0f32, 0.0f32);
            for (v, one) in terms {
                if one {
                    s1 += (v - m1).exp()
                } else {
                    s0 += (v - m0).exp()
                }
            }
            *s = ((m0 + s0.ln()) - (m1 + s1.ln())).clamp(-LLR_MAX, LLR_MAX);
        }
        self.deint.symbols(&mut soft);
        let lat = self.deint.latency() as u64;
        if r >= lat {
            let g = r - lat;
            for p in 0..2 {
                let j = 2 * g + p as u64;
                let (l0, l1) = (soft[2 * p], soft[2 * p + 1]);
                // Bits still to arrive through the deinterleaver are
                // erasures, and not counted against the squelch.
                self.hard
                    .push_back((j, l0 < 0.0, l1 < 0.0, l0 == 0.0 && l1 == 0.0));
                if let Some(bit) = self.viterbi.push(l0, l1) {
                    self.bit(j - VITERBI_DELAY as u64, bit);
                }
            }
            while self.hard.len() > 2 * VITERBI_DELAY as usize + 8 {
                self.hard.pop_front();
            }
        }
    }

    /// The transmitter's symbol period: the line through the instants once
    /// it spans enough symbols, else the loop's.
    fn measured_period(&self) -> f64 {
        let f = &self.fit;
        let den = f[0] * f[3] - f[1] * f[1];
        if f[0] < CLOCK_FIT_SYMS || den <= 0.0 {
            return self.period;
        }
        let slope = (f[0] * f[4] - f[1] * f[2]) / den;
        let l = self.l as f64;
        slope.clamp(l * 0.998, l * 1.002)
    }

    /// The timing error in samples: the profile's peak (to a fraction of a
    /// sample) against where the symbol was due, within an eighth of a
    /// symbol either side.
    fn timing_error(&self, due: f64) -> f64 {
        let l = self.l;
        let reach = (l / 8) as i64;
        let c = due.round() as i64;
        let at = |i: i64| self.profile[i.rem_euclid(l as i64) as usize];
        let (mut bi, mut bv) = (c, f32::MIN);
        for i in c - reach..=c + reach {
            if at(i) > bv {
                bv = at(i);
                bi = i;
            }
        }
        let (a, b, d) = (at(bi - 1), bv, at(bi + 1));
        let den = a - 2.0 * b + d;
        let frac = if den < 0.0 {
            (0.5 * (a - d) / den).clamp(-0.5, 0.5) as f64
        } else {
            0.0
        };
        bi as f64 + frac - due
    }

    /// A decoded data bit, index `j`.
    fn bit(&mut self, j: u64, bit: bool) {
        // Encode it again and compare with what came in.
        let (c0, c1) = self.encoder.push(bit);
        if let Some(&(_, h0, h1, _)) = self.hard.iter().find(|h| h.0 == j && !h.3) {
            let errs = (h0 != c0) as u8 + (h1 != c1) as u8;
            for e in [errs.min(1), errs.saturating_sub(1)] {
                self.errors.push_back(e);
                self.err_sum += e as u32;
            }
            while self.errors.len() > BER_WINDOW {
                self.err_sum -= self.errors.pop_front().unwrap_or(0) as u32;
            }
        }
        let ber = self.ber();
        if self.errors.len() >= BER_WINDOW {
            if !self.open && ber < BER_OPEN {
                self.set_open(true);
                // The window that opened the squelch covers the last
                // BER_WINDOW / 2 data bits, and the decoder's choices a
                // little before them hang on the same path: what they
                // ended prints.
                let from = j.saturating_sub(HELD_BITS);
                let held = std::mem::take(&mut self.held);
                for (k, c) in held {
                    if k >= from {
                        self.vouch.push_back((k, c));
                    }
                }
            } else if self.open && ber > BER_CLOSE {
                self.set_open(false);
            }
        }

        self.release(j);
        self.shreg = (self.shreg << 1) | bit as u32;
        if self.shreg & 7 == 1 {
            let code = self.shreg >> 1;
            self.shreg = 1;
            if let Some(c) = varicode::decode(code) {
                self.character(c, j);
            }
        } else if self.shreg >= 1 << 20 {
            // No code is this long: wait for the next 001.
            self.shreg = 0;
        }
    }

    fn set_open(&mut self, open: bool) {
        if self.open && !open {
            self.shut_since = self.now_s();
        }
        if open {
            self.opened_here = true;
        }
        self.held.clear();
        if !open {
            self.vouch.clear();
        }
        self.open = open;
    }

    /// A decoded character, ended by data bit `j`.
    fn character(&mut self, c: u8, j: u64) {
        let ch = match c {
            b'\r' | b'\n' => '\n',
            0x20..=0x7e => c as char,
            0xa0..=0xff => c as char,
            _ => return,
        };
        if !self.open {
            self.held.push_back((j, ch));
            let from = j.saturating_sub(HELD_BITS);
            while self.held.front().is_some_and(|h| h.0 < from) {
                self.held.pop_front();
            }
            return;
        }
        self.vouch.push_back((j, ch));
        // Pictures: a header starts one at once, without waiting for the
        // bits after it (the flush is all zeros, which the squelch sees
        // soon enough), since its timing is counted from here.
        if ch == '\n' {
            self.recent.clear();
        } else {
            self.recent.push(ch);
            while self.recent.chars().count() > 40 {
                self.recent.remove(0);
            }
        }
        if ch == ';'
            && self.picture.is_none()
            && let Some((w, h, color, spp)) = parse_header(&self.recent)
        {
            self.start_picture(w, h, color, spp, j);
        }
    }

    /// Characters waiting for the bits after them: printed when those
    /// check out, dropped when they do not (the end of a transmission,
    /// where the squelch has not shut yet but noise is all there is).
    fn release(&mut self, now: u64) {
        while let Some(&(j, ch)) = self.vouch.front() {
            if now < j + VOUCH_BITS {
                break;
            }
            self.vouch.pop_front();
            let n = (2 * VOUCH_BITS as usize).min(self.errors.len());
            let bad: u32 = self.errors.iter().rev().take(n).map(|e| *e as u32).sum();
            if !self.open || n == 0 || bad as f32 / n as f32 > BER_CLOSE {
                continue;
            }
            self.print(ch);
        }
    }

    /// Print a character that passed the squelch.
    fn print(&mut self, ch: char) {
        if ch != '\n' {
            self.chars += 1;
        }
        self.last_char = self.now_s();
        if let Some(line) = self.lines.push(ch) {
            self.emit(line);
        }
    }

    /// The header ended at data bit `j` (the 1 that began the flush).
    fn start_picture(&mut self, w: u32, h: u32, color: bool, spp: u32, j: u64) {
        // The flush is that bit and the zeros after it; the symbol they
        // fill last (a half-filled one is dropped) is the last before the
        // lead-in tone.
        let end_group = (j + self.cfg.mode.flush_bits()).div_ceil(2);
        // Transmitted symbol g is received symbol g: the groups count from
        // the same origin as the symbols, the deinterleaver's latency
        // behind them.
        let Some(&(r_last, t_last)) = self.sym_end.back() else {
            return;
        };
        // When received symbol end_group - 1 ends, on the transmitter's
        // clock as the symbols have shown it.
        let period = self.measured_period();
        let t_end = t_last + (end_group as f64 - 1.0 - r_last as f64) * period;
        let ratio = period / self.l as f64;
        let start = t_end + PROLOGUE * ratio;
        if start < self.n as f64 {
            return;
        }
        let total = (w * h * if color { 3 } else { 1 }) as usize;
        let bw = self.cfg.mode.bandwidth_hz() as f64;
        let pixel_rate = RATE / spp as f64;
        self.wide = Some(WideFilter::new((bw / 2.0 + 0.5 * pixel_rate).min(3400.0)));
        self.sym_stop = Some(end_group);
        // The header and what came before it print now, ahead of the
        // picture: the picture's own timing vouches for them.
        while let Some((_, ch)) = self.vouch.pop_front() {
            self.print(ch);
        }
        self.flush_text();
        self.events.push(MfskEvent::PictureStart {
            width: w,
            height: h,
            color,
        });
        self.picture = Some(Picture {
            width: w,
            height: h,
            color,
            start: start + WideFilter::delay(),
            spp: spp as f64 * ratio,
            phase: 0.0,
            span: 0.0,
            index: 0,
            pixels: vec![0; total],
            total,
        });
    }

    /// While a picture is due or coming in: one sample. True while the
    /// picture has the receiver.
    fn picture_sample(&mut self, z: Complex32, n: u64) -> bool {
        let (Some(pic), Some(wide)) = (self.picture.as_mut(), self.wide.as_mut()) else {
            return false;
        };
        let dphi = wide.push(z);
        // This turn covers (n - 1, n].
        let (t0, t1) = (n as f64 - 1.0, n as f64);
        if t1 <= pic.start {
            // Text decoding goes on until the pixels begin (the lead-in
            // and the flush are still symbols).
            return false;
        }
        let bw = self.cfg.mode.bandwidth_hz() as f64;
        let mut t = t0.max(pic.start);
        while pic.index < pic.total {
            let end = pic.start + (pic.index + 1) as f64 * pic.spp;
            if end > t1 {
                let w = t1 - t;
                pic.phase += dphi * w;
                pic.span += w;
                break;
            }
            let w = end - t;
            pic.phase += dphi * w;
            pic.span += w;
            let v = pic.value(bw);
            pic.store(v);
            pic.phase = 0.0;
            pic.span = 0.0;
            t = end;
        }
        if pic.index >= pic.total {
            // The symbols go on right after the last pixel (with the
            // flush), and the code with them: the clock is set from the
            // picture's end, the profile learns afresh.
            let end = pic.start - WideFilter::delay() + pic.total as f64 * pic.spp;
            self.finish_picture(true);
            self.next_sym = Some(end + self.period);
            self.sym_stop = None;
            self.profile.fill(0.0);
            self.profiled = 0;
        }
        true
    }

    fn finish_picture(&mut self, complete: bool) {
        self.wide = None;
        self.sym_stop = None;
        if let Some(p) = self.picture.take() {
            self.events.push(MfskEvent::Picture {
                width: p.width,
                height: p.height,
                color: p.color,
                pixels: p.pixels,
                complete,
            });
        }
    }

    /// Look at the averaged spectrum: find a signal, or a better one. True
    /// when it retuned.
    fn look(&mut self) -> bool {
        // A spectrum averaged over less than a couple of seconds finds the
        // first tones a signal happened to send, not its comb.
        if self.picture.is_some()
            || (self.search.frames * self.search.hop as u64) < (SEARCH_MIN_S * RATE) as u64
        {
            return false;
        }
        let bw = self.cfg.mode.bandwidth_hz() as f64;
        let (lo, hi) = match self.cfg.center_hz {
            Some(c) => (
                c as f64 - bw / 2.0 - SEARCH_NEAR_HZ,
                c as f64 - bw / 2.0 + SEARCH_NEAR_HZ,
            ),
            None => (LO_HZ, HI_HZ - bw),
        };
        let Some((f0, _)) = self.search.best(lo, hi) else {
            return false;
        };
        let c = f0 + bw / 2.0;
        let spacing = self.cfg.mode.baud() as f64;
        match self.center {
            None => {
                self.tune(c);
                true
            }
            Some(cur) => {
                // A receiver that has copied here waits a while after its
                // signal goes; one that never has moves sooner.
                let wait = if self.opened_here {
                    SEARCH_MOVE_AFTER_S
                } else {
                    SEARCH_FIRST_MOVE_S
                };
                let quiet = !self.open
                    && self.now_s() - self.shut_since.max(self.last_char).max(self.tuned_at) > wait;
                if quiet && (c - cur).abs() > spacing / 4.0 {
                    self.tune(c);
                    true
                } else {
                    false
                }
            }
        }
    }

    fn flush_text(&mut self) {
        if let Some(line) = self.lines.take() {
            self.emit(line);
        }
    }

    fn emit(&mut self, text: String) {
        self.events.push(MfskEvent::Text {
            text,
            mode: self.cfg.mode,
            center_hz: self.center.unwrap_or(0.0) as f32,
            snr_db: self.snr_db(),
        });
    }
}

fn ungray(mut g: usize) -> usize {
    let mut x = 0;
    while g != 0 {
        x ^= g;
        g >>= 1;
    }
    x
}

#[cfg(test)]
mod tests;
