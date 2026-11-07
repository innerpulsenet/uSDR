//! Slow-scan television (SSTV) receiver: sideband audio in, color picture
//! rows out.
//!
//! SSTV is an FM subcarrier in the audio passband, like radiofax: black at
//! 1500 Hz, white at 2300 Hz, every level linearly between, and 1200 Hz for
//! the sync pulses below black. A picture opens with the VIS header: a
//! 1900 Hz leader of 300 ms, a 10 ms break at 1200 Hz, another 300 ms of
//! leader, then a 30 ms start bit at 1200 Hz, seven data bits (1100 Hz for
//! a one, 1300 Hz for a zero, least significant first), an even parity bit
//! and a 1200 Hz stop bit, all 30 ms. The seven bits name the mode, and the
//! mode fixes the picture's whole timeline from the end of the stop bit.
//!
//! The audio is mixed down by 1700 Hz, the middle of the 1100..2300 Hz
//! band the modes use, cut to ±850 Hz and taken to 12 kHz. The
//! instantaneous frequency of each sample (the angle between it and the one
//! before) goes into a trace; everything after works on that trace. Each
//! pixel is the mean of the frequency, limited to a little past the
//! black..white range, over the exact time span of that pixel, so pixel
//! edges fall where the mode puts them however they land between samples,
//! and a click from the FM threshold costs one pixel a share of its range at
//! most. No smoothing is added across pixels: the picture is what came in,
//! noise and all.
//!
//! The VIS is looked for on 1 ms means of the trace: a leader at 1900 Hz,
//! give or take a tuning error, then ten 30 ms bits each judged over its
//! middle 20 ms. A candidate is confirmed on medians of the raw samples (a
//! mean is pulled toward the middle of the band by noise, a median much
//! less) and timed to a fraction of a millisecond by the leader's falling
//! edge into the start bit. The leader also gives the tuning error, which is
//! taken off every frequency read after it.
//!
//! The line timing is fitted, not followed. Each line's sync pulse is found
//! near where the fit expects it, by sliding a window of the pulse's length
//! over a sync-likeness signal (1 at 1200 Hz, 0 at black, -1 above) and
//! placing the peak between samples from the slopes either side. The pulse
//! times are fitted with a straight line, weighted by how clean each pulse
//! was, over every line so far: the slope is the true line period, which
//! takes out a sample-clock error between sender and receiver (the slant
//! the picture would otherwise take on), and the intercept is the phase.
//! Every line is drawn from the fit at the time it is drawn, never from its
//! own pulse alone, so one pulse spoiled by noise does not jog one row. A
//! line is drawn once the next line's pulse has been measured, so the fit
//! has seen past it.
//!
//! Color comes three ways. Martin, Scottie and Wraase send green, blue and
//! red (or red, green and blue) scans one after another. Robot 72 sends
//! luminance and both color differences on every line; Robot 36 sends
//! luminance on every line and one color difference on each, R-Y on even
//! lines and B-Y on odd ones (told apart by the separator pulse before them,
//! 1500 Hz or 2300 Hz), so a pair of lines shares its color. PD sends two
//! lines' luminance and the pair's color differences between one sync and
//! the next. The YUV modes are the ITU-R BT.601 studio range (luminance
//! 16..235), as MMSSTV and QSSTV send them.
//!
//! A picture ends when its last line is drawn, on `stop_now`, when a new VIS
//! opens another, or when no sync pulse has been found for several seconds
//! (the sender stopped or the signal is gone). The rows drawn while the
//! decoder waits to be sure of that are what was received: noise.
//!
//! Measured with the test module's transmitter (a test card of color bars
//! and ramps, 50 Hz off tune, a 200 ppm clock error, white noise, SNR in
//! 3 kHz): the VIS decodes down to 3 dB in every mode, and rows stay within
//! a pixel of where they belong. The picture's PSNR against the card passes
//! 20 dB from 5 dB (Scottie DX), 6 dB (Wraase SC2-180), 9 dB (Martin M1,
//! Scottie S1), 11 dB (Martin M2, Scottie S2), 13 to 16 dB (PD), 16 dB
//! (Robot 72) and 19 dB (Robot 36). The YUV modes pay twice: their scans are
//! fast, and turning luminance and color differences into red and blue
//! raises the noise in them by 6 to 7 dB. Robot 36 does not pass 21.5 dB on
//! the card even without noise; its 44 ms color difference scans are faster
//! than the band passes, and the bars' edges blur.

use num_complex::Complex32;
use scannerd_dsp::DecimFir;

/// The rate the discriminator runs at, before rounding to a whole decimation
/// of the input rate. The fastest scan, Robot 36 chroma, has 1.65 samples a
/// pixel here; Martin M2 has 2.7.
const DEC_RATE_HZ: f32 = 12_000.0;
/// The mixer frequency, the middle of the band SSTV uses.
const MIX_HZ: f32 = 1_700.0;
/// One-sided cutoff of the baseband filter: 850..2550 Hz in the audio, which
/// keeps the VIS bits (1100 Hz) and white (2300 Hz). The noise a
/// discriminator passes grows with the cube of the band, and the pixel
/// averaging takes out little of it in the fast modes, so the band is as
/// narrow as the tones allow, with a long filter for a sharp edge. Swept
/// against the test card's PSNR from 16 down to 4 dB in 3 kHz, 850 Hz with
/// 255 taps beat 1100 Hz with 127 by 0.7 dB in Martin M1 and 2.4 dB in
/// PD 120 at 10 dB, edge blur included; 1300 Hz was worse again, and 800 to
/// 900 Hz with 255 taps came within 0.3 dB of each other.
const CUTOFF_HZ: f32 = 850.0;
/// Baseband filter length at a 48 kHz input, scaled with the input rate.
const TAPS_AT_48K: usize = 255;

const SYNC_HZ: f32 = 1_200.0;
const BLACK_HZ: f32 = 1_500.0;
const WHITE_HZ: f32 = 2_300.0;
const LEADER_HZ: f32 = 1_900.0;
/// The trace is limited to this range: a phase slip reads as kilohertz, and
/// nothing past this is SSTV.
const TRACE_LO_HZ: f32 = 700.0;
const TRACE_HI_HZ: f32 = 2_700.0;

/// Samples are limited to this far past black and white before a pixel's
/// mean is taken, so a click costs the pixel little, but noise on a black or
/// white pixel averages out instead of only ever pulling it toward gray
/// (worth 1 dB of PSNR at 10 dB in Martin M1). The mean is limited to
/// black..white after.
const PIXEL_MARGIN_HZ: f32 = 300.0;
/// The most tuning error the VIS detector accepts.
const MAX_OFFSET_HZ: f32 = 150.0;
/// The leader's 10 ms means may scatter this much (standard deviation)
/// before it is taken for something else.
const LEADER_SCATTER_HZ: f32 = 120.0;
/// Start and stop bits must read within this of 1200 Hz (after the tuning
/// error is taken off).
const VIS_SYNC_TOL_HZ: f32 = 90.0;
/// Data bits must read within this band (one below 1200 Hz, zero above).
const VIS_BIT_LO_HZ: f32 = 1_000.0;
const VIS_BIT_HI_HZ: f32 = 1_400.0;
/// ... and at least this far from 1200 Hz.
const VIS_BIT_MARGIN_HZ: f32 = 12.0;

/// A sync pulse is taken when its mean sync-likeness is at least this.
/// Noise centered in the band reads about -0.5 and a black region with
/// noise about 0; a clean pulse reads 1.
const SYNC_ACCEPT: f32 = 0.5;
/// Pulses this clean also measure the tuning error, by the median of their
/// middle half, averaged over the picture. The VIS leader's measurement
/// counts as this many pulses'.
const SYNC_CLEAN: f32 = 0.8;
const VIS_OFFSET_WEIGHT: u32 = 10;
/// How far from the fit a pulse is looked for, before and after three
/// pulses have been found. Nothing within 15 ms of a pulse is below black.
const SEARCH_WIDE_MS: f64 = 15.0;
const SEARCH_NARROW_MS: f64 = 5.0;
/// Once the fit stands on three pulses, a pulse farther than this from it
/// is not believed.
const GATE_MS: f64 = 2.0;
/// The fitted line period may differ from the mode's by this share at
/// most (5000 ppm: no sound card is that far off).
const MAX_SLANT: f64 = 0.005;
/// The picture is given up when no pulse has been found for this long (and
/// at least `LOSS_MIN_LINES` lines).
const LOSS_MS: f64 = 5_000.0;
const LOSS_MIN_LINES: u32 = 8;
/// Samples of trace kept while no picture is running: the VIS check looks
/// back 0.65 s, and `start_now` looks back one line (up to 1.05 s).
const KEEP_IDLE_S: f64 = 2.5;

/// An SSTV mode: one of the timelines a VIS code names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SstvMode {
    MartinM1,
    MartinM2,
    ScottieS1,
    ScottieS2,
    ScottieDx,
    Robot36,
    Robot72,
    Pd50,
    Pd90,
    Pd120,
    Pd160,
    Pd180,
    Pd240,
    Pd290,
    WraaseSc2_180,
}

impl SstvMode {
    pub const ALL: [SstvMode; 15] = [
        SstvMode::MartinM1,
        SstvMode::MartinM2,
        SstvMode::ScottieS1,
        SstvMode::ScottieS2,
        SstvMode::ScottieDx,
        SstvMode::Robot36,
        SstvMode::Robot72,
        SstvMode::Pd50,
        SstvMode::Pd90,
        SstvMode::Pd120,
        SstvMode::Pd160,
        SstvMode::Pd180,
        SstvMode::Pd240,
        SstvMode::Pd290,
        SstvMode::WraaseSc2_180,
    ];

    pub fn name(self) -> &'static str {
        match self {
            SstvMode::MartinM1 => "Martin M1",
            SstvMode::MartinM2 => "Martin M2",
            SstvMode::ScottieS1 => "Scottie S1",
            SstvMode::ScottieS2 => "Scottie S2",
            SstvMode::ScottieDx => "Scottie DX",
            SstvMode::Robot36 => "Robot 36",
            SstvMode::Robot72 => "Robot 72",
            SstvMode::Pd50 => "PD 50",
            SstvMode::Pd90 => "PD 90",
            SstvMode::Pd120 => "PD 120",
            SstvMode::Pd160 => "PD 160",
            SstvMode::Pd180 => "PD 180",
            SstvMode::Pd240 => "PD 240",
            SstvMode::Pd290 => "PD 290",
            SstvMode::WraaseSc2_180 => "Wraase SC2-180",
        }
    }

    /// The seven-bit VIS code (without the parity bit).
    pub fn vis(self) -> u8 {
        match self {
            SstvMode::MartinM1 => 44,
            SstvMode::MartinM2 => 40,
            SstvMode::ScottieS1 => 60,
            SstvMode::ScottieS2 => 56,
            SstvMode::ScottieDx => 76,
            SstvMode::Robot36 => 8,
            SstvMode::Robot72 => 12,
            SstvMode::Pd50 => 93,
            SstvMode::Pd90 => 99,
            SstvMode::Pd120 => 95,
            SstvMode::Pd160 => 98,
            SstvMode::Pd180 => 96,
            SstvMode::Pd240 => 97,
            SstvMode::Pd290 => 94,
            SstvMode::WraaseSc2_180 => 55,
        }
    }

    pub fn from_vis(code: u8) -> Option<SstvMode> {
        SstvMode::ALL.into_iter().find(|m| m.vis() == code)
    }

    /// Pixels per row.
    pub fn width(self) -> u32 {
        match self {
            SstvMode::Pd120 | SstvMode::Pd180 | SstvMode::Pd240 => 640,
            SstvMode::Pd160 => 512,
            SstvMode::Pd290 => 800,
            _ => 320,
        }
    }

    /// Rows in a picture.
    pub fn height(self) -> u32 {
        match self {
            SstvMode::Robot36 | SstvMode::Robot72 => 240,
            SstvMode::Pd120 | SstvMode::Pd180 | SstvMode::Pd240 => 496,
            SstvMode::Pd160 => 400,
            SstvMode::Pd290 => 616,
            _ => 256,
        }
    }

    /// Time from one sync pulse to the next, ms. PD modes send two rows in
    /// that time; every other mode one.
    pub fn line_ms(self) -> f64 {
        self.spec().line_ms
    }

    /// Rows sent between one sync pulse and the next.
    pub fn rows_per_line(self) -> u32 {
        self.spec().rows
    }

    /// The picture's length after the VIS, seconds.
    pub fn duration_s(self) -> f64 {
        let s = self.spec();
        (s.first_sync_ms + s.line_ms * (self.height() / s.rows) as f64) / 1000.0
    }

    fn spec(self) -> Spec {
        use Comp::*;
        let ch = |comp, start_ms, dur_ms| Chan { comp, start_ms, dur_ms };
        // Martin: sync, porch, then green, blue and red each followed by a
        // separator, all at black.
        let martin = |scan: f64| {
            let (sync, gap) = (4.862, 0.572);
            let g = sync + gap;
            Spec {
                sync_ms: sync,
                line_ms: g + 3.0 * (scan + gap),
                rows: 1,
                first_sync_ms: 0.0,
                chans: vec![ch(G, g, scan), ch(B, g + scan + gap, scan), ch(R, g + 2.0 * (scan + gap), scan)],
                separator: None,
            }
        };
        // Scottie: separator, green, separator, blue, sync, porch, red. The
        // pulse sits between blue and red, so green and blue come before the
        // pulse a row is timed from. One extra pulse follows the VIS.
        let scottie = |scan: f64| Spec {
            sync_ms: 9.0,
            line_ms: 13.5 + 3.0 * scan,
            rows: 1,
            first_sync_ms: 9.0 + 3.0 + 2.0 * scan,
            chans: vec![ch(G, -(2.0 * scan + 1.5), scan), ch(B, -scan, scan), ch(R, 10.5, scan)],
            separator: None,
        };
        // PD: sync, porch, then the even row's luminance, R-Y, B-Y and the
        // odd row's luminance.
        let pd = |px_ms: f64, width: u32| {
            let d = px_ms * width as f64;
            let s = 22.08;
            Spec {
                sync_ms: 20.0,
                line_ms: s + 4.0 * d,
                rows: 2,
                first_sync_ms: 0.0,
                chans: vec![ch(Y, s, d), ch(V, s + d, d), ch(U, s + 2.0 * d, d), ch(Y2, s + 3.0 * d, d)],
                separator: None,
            }
        };
        let w = self.width();
        match self {
            SstvMode::MartinM1 => martin(146.432),
            SstvMode::MartinM2 => martin(73.216),
            SstvMode::ScottieS1 => scottie(138.24),
            SstvMode::ScottieS2 => scottie(88.064),
            SstvMode::ScottieDx => scottie(345.6),
            // Sync, porch, luminance, a 4.5 ms separator (1500 Hz before
            // R-Y, 2300 Hz before B-Y), a 1.5 ms porch, one color
            // difference.
            SstvMode::Robot36 => Spec {
                sync_ms: 9.0,
                line_ms: 150.0,
                rows: 1,
                first_sync_ms: 0.0,
                chans: vec![ch(Y, 12.0, 88.0), ch(C, 106.0, 44.0)],
                separator: Some((100.0, 4.5)),
            },
            // Sync, porch, luminance, then each color difference behind its
            // own separator and porch.
            SstvMode::Robot72 => Spec {
                sync_ms: 9.0,
                line_ms: 300.0,
                rows: 1,
                first_sync_ms: 0.0,
                chans: vec![ch(Y, 12.0, 138.0), ch(V, 156.0, 69.0), ch(U, 231.0, 69.0)],
                separator: None,
            },
            SstvMode::Pd50 | SstvMode::Pd180 | SstvMode::Pd290 => pd(0.286, w),
            SstvMode::Pd90 => pd(0.532, w),
            SstvMode::Pd120 => pd(0.19, w),
            SstvMode::Pd160 | SstvMode::Pd240 => pd(0.382, w),
            // Sync, porch, red, green, blue with nothing between.
            SstvMode::WraaseSc2_180 => Spec {
                sync_ms: 5.5225,
                line_ms: 711.0225,
                rows: 1,
                first_sync_ms: 0.0,
                chans: vec![ch(R, 6.0225, 235.0), ch(G, 241.0225, 235.0), ch(B, 476.0225, 235.0)],
                separator: None,
            },
        }
    }
}

/// What a scan carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Comp {
    R,
    G,
    B,
    Y,
    /// PD's second row of luminance.
    Y2,
    /// R-Y.
    V,
    /// B-Y.
    U,
    /// Robot 36's color difference, R-Y or B-Y by the line.
    C,
}

impl Comp {
    fn index(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Copy, Debug)]
struct Chan {
    comp: Comp,
    /// From the start of the line's sync pulse, ms.
    start_ms: f64,
    dur_ms: f64,
}

#[derive(Clone, Debug)]
struct Spec {
    sync_ms: f64,
    /// Sync pulse to sync pulse.
    line_ms: f64,
    /// Rows per line.
    rows: u32,
    /// From the end of the VIS stop bit to the first line's sync pulse.
    first_sync_ms: f64,
    chans: Vec<Chan>,
    /// Robot 36's separator (start and length, ms), whose tone says which
    /// color difference follows.
    separator: Option<(f64, f64)>,
}

impl Spec {
    fn earliest_ms(&self) -> f64 {
        self.chans.iter().map(|c| c.start_ms).fold(0.0, f64::min)
    }

    fn latest_ms(&self) -> f64 {
        self.chans.iter().map(|c| c.start_ms + c.dur_ms).fold(self.sync_ms, f64::max)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SstvState {
    /// Waiting for a VIS (or `start_now`).
    Listening,
    /// A 1900 Hz leader is on: a VIS may follow.
    Vis,
    /// A picture is being received.
    Receiving,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SstvStatus {
    pub state: SstvState,
    /// The picture's mode while one is being received.
    pub mode: Option<SstvMode>,
    /// Rows drawn so far.
    pub line: u32,
    /// Rows in the picture.
    pub lines: u32,
    /// How clean the recent sync pulses were, 0..1 (1 is a clean pulse; a
    /// missed pulse counts 0).
    pub sync_quality: f32,
    /// The fitted line period against the mode's, ppm: the sample-clock
    /// error between sender and receiver being taken out. Positive when
    /// lines arrive longer than they should.
    pub slant_ppm: f32,
    /// Tuning error taken off every frequency, Hz.
    pub offset_hz: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SstvEvent {
    /// A picture has begun, on a VIS (`vis` is its seven-bit code) or
    /// `start_now`.
    Start { mode: SstvMode, width: u32, height: u32, vis: Option<u8> },
    /// One row, `width` pixels of red, green and blue.
    Line { line: u32, rgb: Vec<u8> },
    /// The picture has ended after `lines` rows; `complete` when they were
    /// all of them.
    End { lines: u32, complete: bool },
}

impl SstvEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            SstvEvent::Start { .. } => "start",
            SstvEvent::Line { .. } => "line",
            SstvEvent::End { .. } => "end",
        }
    }

    pub fn summary(&self) -> String {
        match self {
            SstvEvent::Start { mode, width, height, vis } => match vis {
                Some(v) => format!("{} {}x{} (VIS {})", mode.name(), width, height, v),
                None => format!("{} {}x{} (started by hand)", mode.name(), width, height),
            },
            SstvEvent::Line { line, .. } => format!("line {line}"),
            SstvEvent::End { lines, complete: true } => format!("picture complete, {lines} lines"),
            SstvEvent::End { lines, complete: false } => format!("picture ended after {lines} lines"),
        }
    }

    pub fn fields(&self) -> Vec<(&'static str, String)> {
        match self {
            SstvEvent::Start { mode, width, height, vis } => vec![
                ("mode", mode.name().to_string()),
                ("width", width.to_string()),
                ("height", height.to_string()),
                ("vis", vis.map(|v| v.to_string()).unwrap_or_default()),
            ],
            SstvEvent::Line { line, .. } => vec![("line", line.to_string())],
            SstvEvent::End { lines, complete } => {
                vec![("lines", lines.to_string()), ("complete", complete.to_string())]
            }
        }
    }
}

/// Weighted least-squares line through the sync pulse times. `y` is a
/// pulse's time less where the mode's nominal period puts it, so the sums
/// stay small; the fit is `y = a + b k`.
#[derive(Default, Clone, Debug)]
struct LineFit {
    sw: f64,
    swk: f64,
    swkk: f64,
    swy: f64,
    swky: f64,
    n: u32,
    k_lo: u32,
    k_hi: u32,
    a: f64,
    b: f64,
}

impl LineFit {
    fn add(&mut self, k: u32, y: f64, w: f64, max_b: f64) {
        let kf = k as f64;
        self.sw += w;
        self.swk += w * kf;
        self.swkk += w * kf * kf;
        self.swy += w * y;
        self.swky += w * kf * y;
        if self.n == 0 {
            self.k_lo = k;
        }
        self.k_hi = k;
        self.n += 1;
        // A slope from pulses only a line or two apart is mostly their
        // noise; until they span three lines only the phase is fitted.
        if self.n >= 3 && self.k_hi - self.k_lo >= 3 {
            let det = self.sw * self.swkk - self.swk * self.swk;
            if det > 1e-9 {
                self.b = ((self.sw * self.swky - self.swk * self.swy) / det).clamp(-max_b, max_b);
            }
        }
        self.a = (self.swy - self.b * self.swk) / self.sw;
    }

    fn at(&self, k: u32) -> f64 {
        self.a + self.b * k as f64
    }
}

/// Robot 36 rows waiting for the other half of their color.
#[derive(Default)]
struct Robot36 {
    /// An R-Y row (line number, luminance, R-Y) waiting for its B-Y.
    pending: Option<(u32, Vec<f32>, Vec<f32>)>,
    last_v: Option<Vec<f32>>,
    last_u: Option<Vec<f32>>,
}

struct Picture {
    mode: SstvMode,
    spec: Spec,
    /// Nominal line period, decimated samples.
    p_nom: f64,
    /// Where the nominal timeline puts line 0's sync pulse.
    origin: f64,
    fit: LineFit,
    /// The next line whose pulse is to be measured, and drawn.
    next_measure: u32,
    next_emit: u32,
    /// Lines (sync periods) in the picture.
    periods: u32,
    rows_out: u32,
    misses: u32,
    loss_limit: u32,
    quality: f32,
    /// Clean pulses that have measured the tuning error.
    off_n: u32,
    r36: Robot36,
}

impl Picture {
    fn period(&self) -> f64 {
        self.p_nom + self.fit.b
    }

    fn scale(&self) -> f64 {
        self.period() / self.p_nom
    }

    /// Where line `k`'s sync pulse begins, by the fit.
    fn sync_at(&self, k: u32) -> f64 {
        self.origin + k as f64 * self.p_nom + self.fit.at(k)
    }
}

pub struct SstvDecoder {
    fs: f32,
    fs_dec: f64,
    /// Decimated samples per millisecond.
    spms: f64,
    hz_per_rad: f32,

    phasor: Complex32,
    step: Complex32,
    since_norm: u32,
    fir: DecimFir,
    mixed: Vec<Complex32>,
    base: Vec<Complex32>,
    prev: Complex32,

    /// Instantaneous frequency, Hz, of decimated samples `trace_base..n`.
    trace: Vec<f32>,
    trace_base: u64,
    n: u64,

    /// 1 ms means of the trace for the VIS search: `bin` samples each,
    /// bin `j` covering samples `j * bin ..`.
    bin: u32,
    bins_per_ms: f64,
    bin_acc: f32,
    bin_k: u32,
    bins: Vec<f32>,
    bins_base: u64,
    /// No VIS is looked for before this bin (the one just decoded is still
    /// in the window).
    vis_hold: u64,
    leader: bool,

    offset_hz: f32,
    pic: Option<Picture>,
    /// `start_now` waiting to find the line phase: the mode, and the sample
    /// the search starts at.
    acquire: Option<(SstvMode, u64)>,
    events: Vec<SstvEvent>,
}

impl SstvDecoder {
    pub fn new(fs: f32) -> Self {
        let decim = ((fs / DEC_RATE_HZ).round() as usize).max(1);
        let fs_dec = fs as f64 / decim as f64;
        let ntaps = ((TAPS_AT_48K as f32 * fs / 48_000.0).round() as usize).max(15) | 1;
        let bin = (fs_dec / 1000.0).round().max(1.0) as u32;
        let w = -std::f64::consts::TAU * MIX_HZ as f64 / fs as f64;
        Self {
            fs,
            fs_dec,
            spms: fs_dec / 1000.0,
            hz_per_rad: (fs_dec / std::f64::consts::TAU) as f32,
            phasor: Complex32::new(1.0, 0.0),
            step: Complex32::new(w.cos() as f32, w.sin() as f32),
            since_norm: 0,
            fir: DecimFir::new(CUTOFF_HZ, fs, decim, ntaps),
            mixed: Vec::new(),
            base: Vec::new(),
            prev: Complex32::new(0.0, 0.0),
            trace: Vec::new(),
            trace_base: 0,
            n: 0,
            bin,
            bins_per_ms: fs_dec / 1000.0 / bin as f64,
            bin_acc: 0.0,
            bin_k: 0,
            bins: Vec::new(),
            bins_base: 0,
            vis_hold: 0,
            leader: false,
            offset_hz: 0.0,
            pic: None,
            acquire: None,
            events: Vec::new(),
        }
    }

    /// The input rate this decoder was made for.
    pub fn fs(&self) -> f32 {
        self.fs
    }

    pub fn status(&self) -> SstvStatus {
        let (mode, line, lines, q, slant) = match (&self.pic, &self.acquire) {
            (Some(p), _) => (Some(p.mode), p.rows_out, p.mode.height(), p.quality, (p.fit.b / p.p_nom * 1e6) as f32),
            (None, Some((m, _))) => (Some(*m), 0, m.height(), 0.0, 0.0),
            (None, None) => (None, 0, 0, 0.0, 0.0),
        };
        let state = if mode.is_some() {
            SstvState::Receiving
        } else if self.leader {
            SstvState::Vis
        } else {
            SstvState::Listening
        };
        SstvStatus { state, mode, line, lines, sync_quality: q, slant_ppm: slant, offset_hz: self.offset_hz }
    }

    /// Start receiving a `mode` picture whose VIS was missed. The line phase
    /// is found from the sync pulses of the last line and the next two, and
    /// rows are numbered from the first line found. Any picture underway
    /// ends.
    pub fn start_now(&mut self, mode: SstvMode) {
        self.finish(false);
        let p = mode.line_ms() * self.spms;
        let from = (self.n as f64 - p).max(self.trace_base as f64) as u64;
        self.acquire = Some((mode, from));
        self.events.push(SstvEvent::Start { mode, width: mode.width(), height: mode.height(), vis: None });
    }

    /// End the picture underway, if any.
    pub fn stop_now(&mut self) {
        self.finish(false);
    }

    pub fn take(&mut self) -> Vec<SstvEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn push(&mut self, audio: &[f32]) {
        let mut mixed = std::mem::take(&mut self.mixed);
        let mut base = std::mem::take(&mut self.base);
        mixed.clear();
        mixed.reserve(audio.len());
        for &x in audio {
            mixed.push(self.phasor * x);
            self.phasor *= self.step;
            self.since_norm += 1;
            if self.since_norm >= 1024 {
                self.since_norm = 0;
                // Newton step toward |phasor| = 1, cheaper than a square root.
                self.phasor *= 1.5 - 0.5 * self.phasor.norm_sqr();
            }
        }
        self.fir.process(&mixed, &mut base);
        for &z in &base {
            let d = z * self.prev.conj();
            self.prev = z;
            let f = (MIX_HZ + d.im.atan2(d.re) * self.hz_per_rad).clamp(TRACE_LO_HZ, TRACE_HI_HZ);
            self.trace.push(f);
            self.n += 1;
            self.bin_acc += f;
            self.bin_k += 1;
            if self.bin_k == self.bin {
                self.bins.push(self.bin_acc / self.bin as f32);
                self.bin_acc = 0.0;
                self.bin_k = 0;
                self.on_bin();
                self.advance();
            }
        }
        self.mixed = mixed;
        self.base = base;
        self.advance();
        self.trim();
    }

    fn bins_end(&self) -> u64 {
        self.bins_base + self.bins.len() as u64
    }

    /// Mean of bins `lo..hi` (absolute), or `None` if they are not all
    /// held.
    fn bin_mean(&self, lo: i64, hi: i64) -> Option<f32> {
        if lo < self.bins_base as i64 || hi > self.bins_end() as i64 || hi <= lo {
            return None;
        }
        let a = (lo - self.bins_base as i64) as usize;
        let b = (hi - self.bins_base as i64) as usize;
        Some(self.bins[a..b].iter().sum::<f32>() / (b - a) as f32)
    }

    /// Mean and scatter (standard deviation of 10 ms means) of bins
    /// `lo..hi`.
    fn leader_stats(&self, lo: i64, hi: i64) -> Option<(f32, f32)> {
        let mean = self.bin_mean(lo, hi)?;
        let step = (10.0 * self.bins_per_ms).round().max(1.0) as i64;
        let (mut sq, mut n) = (0f32, 0f32);
        let mut j = lo;
        while j + step <= hi {
            let m = self.bin_mean(j, j + step)?;
            sq += (m - mean) * (m - mean);
            n += 1.0;
            j += step;
        }
        Some((mean, if n > 0.0 { (sq / n).sqrt() } else { 0.0 }))
    }

    fn on_bin(&mut self) {
        let end = self.bins_end() as i64;
        let r = self.bins_per_ms;
        self.leader = self
            .leader_stats(end - (100.0 * r).round() as i64, end)
            .is_some_and(|(m, s)| (m - LEADER_HZ).abs() <= MAX_OFFSET_HZ && s <= LEADER_SCATTER_HZ);
        if (end as u64) < self.vis_hold {
            return;
        }
        if let Some((mode, code, off, edge)) = self.find_vis(end) {
            // The picture starts at the end of the stop bit, 300 ms after
            // the start bit's leading edge.
            let t_pic = edge + 300.0 * self.spms;
            self.vis_hold = ((t_pic / self.bin as f64) + 400.0 * r) as u64;
            self.finish(false);
            self.acquire = None;
            self.offset_hz = off;
            self.events.push(SstvEvent::Start { mode, width: mode.width(), height: mode.height(), vis: Some(code) });
            let spec = mode.spec();
            let origin = t_pic + spec.first_sync_ms * self.spms;
            // Line 0's pulse runs straight on from the 1200 Hz stop bit when
            // nothing comes between them, so it has no leading edge to time;
            // the VIS times that line.
            let skip_first = spec.first_sync_ms == 0.0;
            self.begin(mode, origin, skip_first);
            // 190 ms of leader measure the tuning error better than one
            // pulse; the pulses refine it from there.
            if let Some(pic) = self.pic.as_mut() {
                pic.off_n = VIS_OFFSET_WEIGHT;
            }
        }
    }

    /// The VIS ending at bin `end`, if there is one: (mode, code, tuning
    /// error, the start bit's leading edge in samples).
    fn find_vis(&self, end: i64) -> Option<(SstvMode, u8, f32, f64)> {
        let r = self.bins_per_ms;
        let s = end as f64 - 300.0 * r;
        let at = |ms: f64| (s + ms * r).round() as i64;
        let (lead, scatter) = self.leader_stats(at(-200.0), at(-10.0))?;
        let off = lead - LEADER_HZ;
        if off.abs() > MAX_OFFSET_HZ || scatter > LEADER_SCATTER_HZ {
            return None;
        }
        // A quick look on the bin means first; the medians below cost more.
        let mut bits = [0f32; 10];
        for (i, b) in bits.iter_mut().enumerate() {
            let t = 30.0 * i as f64;
            *b = self.bin_mean(at(t + 5.0), at(t + 25.0))? - off;
        }
        decode_vis_bits(&bits)?;
        // Confirm on medians of the samples.
        let s_samp = s * self.bin as f64;
        let mut scratch = Vec::new();
        for (i, b) in bits.iter_mut().enumerate() {
            let t = 30.0 * i as f64;
            let lo = (s_samp + (t + 5.0) * self.spms).round() as u64;
            let hi = (s_samp + (t + 25.0) * self.spms).round() as u64;
            scratch.clear();
            scratch.extend_from_slice(self.trace_span(lo, hi)?);
            *b = median(&mut scratch) - off;
        }
        let code = decode_vis_bits(&bits)?;
        let mode = SstvMode::from_vis(code)?;
        // The leader's falling edge: across a window holding leader then
        // start bit, each sample's share of the way from 1200 to 1900 Hz
        // sums to the leader's length in it.
        let w = 15.0 * self.spms;
        let lo = (s_samp - w).round() as u64;
        let hi = (s_samp + w).round() as u64;
        let span = self.trace_span(lo, hi)?;
        let high: f64 =
            span.iter().map(|&f| ((f - off - SYNC_HZ) / (LEADER_HZ - SYNC_HZ)).clamp(0.0, 1.0) as f64).sum();
        Some((mode, code, off, lo as f64 + high))
    }

    fn trace_span(&self, lo: u64, hi: u64) -> Option<&[f32]> {
        if lo < self.trace_base || hi > self.n || hi <= lo {
            return None;
        }
        Some(&self.trace[(lo - self.trace_base) as usize..(hi - self.trace_base) as usize])
    }

    fn begin(&mut self, mode: SstvMode, origin: f64, skip_first: bool) {
        let spec = mode.spec();
        let p_nom = spec.line_ms * self.spms;
        let periods = mode.height() / spec.rows;
        let loss_limit = ((LOSS_MS / spec.line_ms).ceil() as u32).max(LOSS_MIN_LINES);
        self.pic = Some(Picture {
            mode,
            spec,
            p_nom,
            origin,
            fit: LineFit::default(),
            next_measure: u32::from(skip_first),
            next_emit: 0,
            periods,
            rows_out: 0,
            misses: 0,
            loss_limit,
            quality: 0.0,
            off_n: 0,
            r36: Robot36::default(),
        });
    }

    /// Measure pulses and draw lines as far as the trace allows.
    fn advance(&mut self) {
        if let Some((mode, from)) = self.acquire
            && !self.try_acquire(mode, from)
        {
            return;
        }
        loop {
            let Some(pic) = &self.pic else { return };
            if pic.next_emit >= pic.periods {
                self.finish(true);
                return;
            }
            let k = pic.next_measure;
            if k <= pic.next_emit + 1 {
                let (lo, hi, len) = self.search_window(pic, k);
                if hi + len as i64 + 2 >= self.n as i64 {
                    return;
                }
                let found = self.find_sync(lo, hi, len);
                self.take_sync(k, found);
                continue;
            }
            let k = pic.next_emit;
            let end = pic.sync_at(k) + pic.spec.latest_ms() * self.spms * pic.scale() + 2.0;
            if end >= self.n as f64 {
                return;
            }
            let rows = self.decode_line(k);
            let pic = self.pic.as_mut().expect("picture");
            pic.next_emit += 1;
            for (line, rgb) in rows {
                pic.rows_out += 1;
                self.events.push(SstvEvent::Line { line, rgb });
            }
        }
    }

    /// The samples a line's pulse is looked for in: start positions
    /// `lo..=hi` and the pulse length.
    fn search_window(&self, pic: &Picture, k: u32) -> (i64, i64, usize) {
        let c = pic.sync_at(k);
        let w = if pic.fit.n >= 3 { SEARCH_NARROW_MS } else { SEARCH_WIDE_MS } * self.spms;
        let len = (pic.spec.sync_ms * self.spms * pic.scale()).round().max(2.0) as usize;
        let lo = ((c - w).floor() as i64).max(self.trace_base as i64 + 1);
        (lo, (c + w).ceil() as i64, len)
    }

    /// The best pulse start in `lo..=hi` (fractional sample) and its mean
    /// sync-likeness.
    fn find_sync(&self, lo: i64, hi: i64, len: usize) -> Option<(f64, f32)> {
        if hi < lo {
            return None;
        }
        let off = self.offset_hz;
        let first = (lo - 1 - self.trace_base as i64) as usize;
        let last = (hi + 1 - self.trace_base as i64) as usize + len;
        let span = self.trace.get(first..last)?;
        let mut prefix = Vec::with_capacity(span.len() + 1);
        let mut acc = 0f64;
        prefix.push(0.0);
        for &f in span {
            acc += ((BLACK_HZ + off - f) / (BLACK_HZ - SYNC_HZ)).clamp(-1.0, 1.0) as f64;
            prefix.push(acc);
        }
        // Box sum of the pulse's length starting at span index i.
        let boxed = |i: usize| prefix[i + len] - prefix[i];
        let count = (hi - lo + 1) as usize;
        let (mut best_i, mut best) = (1usize, f64::MIN);
        for i in 1..=count {
            let v = boxed(i);
            if v > best {
                best = v;
                best_i = i;
            }
        }
        // The box sum peaks in a triangle as the window slides over the
        // pulse; the slopes either side place the edge between samples.
        let (sm, sp) = (boxed(best_i - 1), boxed(best_i + 1));
        let drop = best - sm.min(sp);
        let frac = if drop > 1e-9 { ((sp - sm) / (2.0 * drop)).clamp(-0.5, 0.5) } else { 0.0 };
        let q = (best / len as f64) as f32;
        Some(((lo - 1) as f64 + best_i as f64 + frac, q))
    }

    fn take_sync(&mut self, k: u32, found: Option<(f64, f32)>) {
        let spms = self.spms;
        let mut clean_at = None;
        {
            let pic = self.pic.as_mut().expect("picture");
            pic.next_measure += 1;
            let in_picture = k < pic.periods;
            let mut ok = false;
            if let Some((t, q)) = found {
                let y = t - (pic.origin + k as f64 * pic.p_nom);
                let resid = y - pic.fit.at(k);
                let gated = pic.fit.n >= 3 && resid.abs() > GATE_MS * spms;
                if q >= SYNC_ACCEPT && !gated {
                    ok = true;
                    pic.fit.add(k, y, (q * q) as f64, pic.p_nom * MAX_SLANT);
                    if q >= SYNC_CLEAN {
                        clean_at = Some((t, pic.spec.sync_ms * spms * pic.scale()));
                    }
                }
                if in_picture {
                    pic.quality = 0.85 * pic.quality + 0.15 * if ok { q } else { 0.0 };
                }
            } else if in_picture {
                pic.quality *= 0.85;
            }
            if in_picture {
                if ok {
                    pic.misses = 0;
                } else {
                    pic.misses += 1;
                }
                if pic.misses >= pic.loss_limit {
                    self.finish(false);
                    return;
                }
            }
        }
        // A clean pulse measures the tuning error: the median of its middle
        // half, which a click or two does not move.
        if let Some((t, len)) = clean_at {
            let lo = (t + 0.25 * len).round() as u64;
            let hi = (t + 0.75 * len).round() as u64;
            if let Some(span) = self.trace_span(lo, hi) {
                let mut v = span.to_vec();
                let est = median(&mut v) - SYNC_HZ;
                if est.abs() <= MAX_OFFSET_HZ {
                    let pic = self.pic.as_mut().expect("picture");
                    pic.off_n += 1;
                    let gain = (1.0 / pic.off_n as f32).max(0.05);
                    self.offset_hz += gain * (est - self.offset_hz);
                }
            }
        }
    }

    /// Mean frequency over samples `a..b` (fractional), each sample limited
    /// to `lo..hi` first and weighted by how much of it the span covers.
    fn span_mean(&self, a: f64, b: f64, lo: f32, hi: f32) -> f32 {
        let base = self.trace_base as i64;
        let (mut acc, mut w) = (0f64, 0f64);
        let mut i = a.floor() as i64;
        let end = b.ceil() as i64;
        while i < end {
            let s0 = (i as f64).max(a);
            let s1 = ((i + 1) as f64).min(b);
            let idx = i - base;
            if s1 > s0 && idx >= 0 && (idx as usize) < self.trace.len() {
                acc += self.trace[idx as usize].clamp(lo, hi) as f64 * (s1 - s0);
                w += s1 - s0;
            }
            i += 1;
        }
        if w > 0.0 { (acc / w) as f32 } else { 0.5 * (lo + hi) }
    }

    /// One scan of `width` pixels, levels 0..255 (unrounded).
    fn scan(&self, t0: f64, dur: f64, width: usize) -> Vec<f32> {
        let black = BLACK_HZ + self.offset_hz;
        let (lo, hi) = (black - PIXEL_MARGIN_HZ, WHITE_HZ + self.offset_hz + PIXEL_MARGIN_HZ);
        let k = 255.0 / (WHITE_HZ - BLACK_HZ);
        let pw = dur / width as f64;
        (0..width)
            .map(|j| {
                let a = t0 + j as f64 * pw;
                (self.span_mean(a, a + pw, lo, hi) - black) * k
            })
            .collect()
    }

    /// Decode line `k` into the rows it completes (none, one or two).
    fn decode_line(&mut self, k: u32) -> Vec<(u32, Vec<u8>)> {
        let pic = self.pic.as_ref().expect("picture");
        let width = pic.mode.width() as usize;
        let tau = pic.sync_at(k);
        let ms = self.spms * pic.scale();
        let mut ch: [Option<Vec<f32>>; 8] = Default::default();
        for c in &pic.spec.chans {
            ch[c.comp.index()] = Some(self.scan(tau + c.start_ms * ms, c.dur_ms * ms, width));
        }
        let is_v = pic.spec.separator.map(|(s, d)| {
            let f = self.span_mean(tau + s * ms, tau + (s + d) * ms, 1_400.0, 2_400.0) - self.offset_hz;
            // Unclear (noise), go by the line's parity from the VIS.
            if (f - LEADER_HZ).abs() < 150.0 { k.is_multiple_of(2) } else { f < LEADER_HZ }
        });
        let rows = pic.spec.rows;
        let mut take = |c: Comp| ch[c.index()].take().unwrap_or_else(|| vec![128.0; width]);
        if let Some(is_v) = is_v {
            let (y, c) = (take(Comp::Y), take(Comp::C));
            return self.robot36(k, y, c, is_v);
        }
        if pic.spec.chans.iter().any(|c| c.comp == Comp::R) {
            let (r, g, b) = (take(Comp::R), take(Comp::G), take(Comp::B));
            let mut rgb = Vec::with_capacity(width * 3);
            for j in 0..width {
                rgb.extend([r[j], g[j], b[j]].map(level));
            }
            return vec![(k, rgb)];
        }
        let (u, v) = (take(Comp::U), take(Comp::V));
        let mut out = vec![(k * rows, yuv_row(&take(Comp::Y), &u, &v))];
        if rows == 2 {
            out.push((k * 2 + 1, yuv_row(&take(Comp::Y2), &u, &v)));
        }
        out
    }

    /// Pair Robot 36 rows: an R-Y row waits for the B-Y row after it, and
    /// the two are drawn with both. A row whose partner is missing borrows
    /// the latest of the other kind (or none, gray, if there is none yet).
    fn robot36(&mut self, k: u32, y: Vec<f32>, c: Vec<f32>, is_v: bool) -> Vec<(u32, Vec<u8>)> {
        let r = &mut self.pic.as_mut().expect("picture").r36;
        let neutral = vec![128.0; y.len()];
        let mut out = Vec::new();
        if is_v {
            if let Some((line, py, pv)) = r.pending.take() {
                out.push((line, yuv_row(&py, r.last_u.as_ref().unwrap_or(&neutral), &pv)));
            }
            r.last_v = Some(c.clone());
            r.pending = Some((k, y, c));
        } else {
            match r.pending.take() {
                Some((line, py, pv)) => {
                    out.push((line, yuv_row(&py, &c, &pv)));
                    out.push((k, yuv_row(&y, &c, &pv)));
                }
                None => out.push((k, yuv_row(&y, &c, r.last_v.as_ref().unwrap_or(&neutral)))),
            }
            r.last_u = Some(c);
        }
        out
    }

    /// End the picture underway (if any) after drawing what is held.
    fn finish(&mut self, complete: bool) {
        self.acquire = None;
        let Some(mut pic) = self.pic.take() else { return };
        if let Some((line, y, v)) = pic.r36.pending.take() {
            let neutral = vec![128.0; y.len()];
            let rgb = yuv_row(&y, pic.r36.last_u.as_ref().unwrap_or(&neutral), &v);
            pic.rows_out += 1;
            self.events.push(SstvEvent::Line { line, rgb });
        }
        let complete = complete && pic.rows_out >= pic.mode.height();
        self.events.push(SstvEvent::End { lines: pic.rows_out, complete });
    }

    /// For `start_now`: once three lines from `from` are held, fold their
    /// sync-likeness at the mode's period and take the strongest pulse as
    /// the line phase. Returns whether a picture is running.
    fn try_acquire(&mut self, mode: SstvMode, from: u64) -> bool {
        let spec = mode.spec();
        let p = spec.line_ms * self.spms;
        let len = (spec.sync_ms * self.spms).round().max(2.0) as usize;
        let pn = p.round() as usize;
        if (self.n as f64) < from as f64 + 3.0 * p + len as f64 + 2.0 || from < self.trace_base {
            return false;
        }
        let off = self.offset_hz;
        let mut fold = vec![0f64; pn];
        for m in 0..3 {
            let start = from as f64 + m as f64 * p;
            for (j, acc) in fold.iter_mut().enumerate() {
                let i = (start + j as f64).round() as u64 - self.trace_base;
                let f = self.trace[i as usize];
                *acc += ((BLACK_HZ + off - f) / (BLACK_HZ - SYNC_HZ)).clamp(-1.0, 1.0) as f64;
            }
        }
        let mut sum: f64 = fold[..len].iter().sum();
        let (mut best_j, mut best) = (0usize, sum);
        for j in 1..pn {
            sum += fold[(j + len - 1) % pn] - fold[j - 1];
            if sum > best {
                best = sum;
                best_j = j;
            }
        }
        self.acquire = None;
        let mut origin = from as f64 + best_j as f64;
        // A line whose scans start before its pulse (Scottie) needs them
        // held.
        while origin + spec.earliest_ms() * self.spms < self.trace_base as f64 + 2.0 {
            origin += p;
        }
        self.begin(mode, origin, false);
        true
    }

    /// Drop trace no longer needed.
    fn trim(&mut self) {
        let mut keep = self.n as f64 - KEEP_IDLE_S * self.fs_dec;
        if let Some(pic) = &self.pic {
            let k = pic.next_measure.min(pic.next_emit);
            let early = pic.sync_at(k) + pic.spec.earliest_ms().min(0.0) * self.spms * pic.scale();
            keep = keep.min(early - SEARCH_WIDE_MS * self.spms - 4.0);
        }
        if let Some((_, from)) = self.acquire {
            keep = keep.min(from as f64);
        }
        let keep = keep.max(0.0) as u64;
        if keep > self.trace_base + self.fs_dec as u64 {
            let cut = (keep - self.trace_base) as usize;
            self.trace.drain(..cut.min(self.trace.len()));
            self.trace_base += cut as u64;
        }
        // The VIS looks back 650 ms; keep a second of bins.
        let keep_bins = self.bins_end().saturating_sub((1000.0 * self.bins_per_ms) as u64);
        if keep_bins > self.bins_base + 1000 {
            let cut = (keep_bins - self.bins_base) as usize;
            self.bins.drain(..cut);
            self.bins_base += cut as u64;
        }
    }
}

/// Start bit, seven data bits, parity, stop bit (Hz, tuning error taken
/// off) to the seven-bit code, if they make one.
fn decode_vis_bits(bits: &[f32; 10]) -> Option<u8> {
    if (bits[0] - SYNC_HZ).abs() > VIS_SYNC_TOL_HZ || (bits[9] - SYNC_HZ).abs() > VIS_SYNC_TOL_HZ {
        return None;
    }
    let mut code = 0u8;
    let mut ones = 0;
    for (i, &f) in bits[1..9].iter().enumerate() {
        if !(VIS_BIT_LO_HZ..=VIS_BIT_HI_HZ).contains(&f) || (f - SYNC_HZ).abs() < VIS_BIT_MARGIN_HZ {
            return None;
        }
        // 1100 Hz is a one, 1300 Hz a zero.
        if f < SYNC_HZ {
            ones += 1;
            if i < 7 {
                code |= 1 << i;
            }
        }
    }
    (ones % 2 == 0).then_some(code)
}

fn median(v: &mut [f32]) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    let mid = v.len() / 2;
    *v.select_nth_unstable_by(mid, f32::total_cmp).1
}

fn level(v: f32) -> u8 {
    v.round().clamp(0.0, 255.0) as u8
}

/// ITU-R BT.601 studio range to RGB.
fn yuv_row(y: &[f32], u: &[f32], v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(y.len() * 3);
    for j in 0..y.len() {
        let l = 1.164 * (y[j] - 16.0);
        let (cb, cr) = (u[j] - 128.0, v[j] - 128.0);
        out.extend([l + 1.596 * cr, l - 0.392 * cb - 0.813 * cr, l + 2.017 * cb].map(level));
    }
    out
}

#[cfg(test)]
mod tests;
