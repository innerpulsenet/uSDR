//! WWV and WWVH: the NIST time stations at Fort Collins, Colorado, and
//! Kauai, Hawaii, on 2.5, 5, 10, 15, 20 and 25 MHz (WWVH 2.5 to 15).
//!
//! What they send (NIST Special Publication 432 and 250-67):
//!
//! - **An AM carrier with second ticks.** Each second starts with a tick,
//!   5 ms of 1000 Hz at WWV or 1200 Hz at WWVH at 100 % modulation, inside
//!   a 40 ms protected zone (10 ms before the second to 30 ms after it) in
//!   which nothing else is sent. The ticks of seconds 29 and 59 are left
//!   out. Second 0 carries an 800 ms tone instead, at the tick frequency,
//!   or at 1500 Hz on both stations at the top of the hour. The start of
//!   the tick is the on-time marker.
//! - **A time code on a 100 Hz subcarrier**, one symbol a second. The
//!   subcarrier sits at 3 % modulation, rises to 18 % at the end of the
//!   protected zone, 30 ms into the second, and drops back 200 ms (a 0),
//!   500 ms (a 1) or 800 ms (a position marker) after the second. Markers
//!   fall at seconds 9, 19, 29, 39, 49 and 59, and second 0 has no pulse at
//!   all, so a marker followed by a missing pulse marks the minute. The
//!   frame gives the UTC at its start, in BCD with the least significant
//!   bit first; unused bits are always 0:
//!
//!   | second | field |
//!   |---|---|
//!   | 2 | DST1 |
//!   | 3 | leap second at the end of this month |
//!   | 4–7 | year units (1 2 4 8) |
//!   | 10–13, 15–17 | minute units, tens (10 20 40) |
//!   | 20–23, 25–26 | hour units, tens (10 20) |
//!   | 30–33, 35–38, 40–41 | day of year units, tens, hundreds |
//!   | 50 | DUT1 sign, 1 = positive |
//!   | 51–54 | year tens |
//!   | 55 | DST2 |
//!   | 56–58 | DUT1 magnitude in tenths of a second (1 2 4) |
//!
//!   DST2 changes at 00:00 UTC on the day daylight time starts or ends,
//!   DST1 24 hours later, so the pair says standard time (both 0),
//!   daylight time (both 1), or that the change is today (they differ).
//!
//! How it is decoded:
//!
//! - **Coherent AM detection.** The I/Q is cut to about 4 kHz. A
//!   frequency-locked loop takes out the carrier's offset, and the carrier
//!   itself, averaged over 50 ms, is the phase reference and the level:
//!   each 10 ms block is demodulated against it and divided by its power,
//!   so the result is the modulation depth whatever the input level, and
//!   it stays linear at carrier-to-noise ratios where an envelope detector
//!   has broken down. Exactly one cycle of 100 Hz fits in a block, which
//!   also removes the carrier, the ticks and the 500, 600, 1000 and
//!   1500 Hz tones.
//! - **The 100 Hz code.** Each block's 100 Hz phasor is projected on the
//!   subcarrier's own phase, tracked over a couple of seconds. Folding the
//!   result on the local second, over about ten seconds, shows the code's
//!   shape; its rising edge out of the silent protected zone gives the
//!   second's start. Each second is then judged by the subcarrier's level
//!   in three windows (where every pulse is high, where only 1s and
//!   markers are, where only markers are), as log-likelihoods for a
//!   missing pulse, 0, 1 or marker. Each block counts in proportion to
//!   the carrier's power, so a second heard through a fade counts for
//!   less, not as noise read with confidence.
//! - **The minute.** Of the 60 ways to align the last minute of seconds
//!   on a frame, the one whose markers, missing pulse and unused bits fit
//!   best by a clear margin is taken. A frame is read when its marker 59
//!   ends, unless its structure does not fit or an unused bit is clearly
//!   set. On HF a fade costs a few bits a minute, and one wrong bit in 38
//!   loses a minute read alone, so the time is read from up to ten
//!   minutes of frames together: the time, counted back minute by minute,
//!   that best fits all their soft bits, with every BCD digit and the date
//!   in range (see [`combine`]). Its confidence bounds the chance that
//!   any field is wrong. A minute is reported only at 99 % or more, and
//!   only when its own frame was heard and does not contradict the
//!   reading.
//! - **Ticks and stations.** Matched filters for a 5 ms burst at 1000 and
//!   1200 Hz, one for each sideband and summed, so a carrier fade or
//!   selective fading cannot hide the tick, are folded on the second. A
//!   station is heard when its fold has a peak well over its median, and
//!   that peak is not the other station's tick or tone onset leaking in (a
//!   5 ms burst is only 200 Hz wide). With wall-clock timestamps, the
//!   peak's arrival relative to the UTC second is reported: the propagation
//!   delay plus the local clock's error. The sample clock's drift against
//!   the station's is measured from where the ticks (or the code's edge)
//!   arrive over the last half minute; the tick folds turn with the
//!   station's second as measured, so a tick neither smears nor lags in
//!   them, and the code fold's lag is corrected by the drift.
//!
//! Not handled: the leap second (a 61-second minute; the frame realigns a
//! minute or two later), the voice announcements (assumed not to disturb
//! the 100 Hz code), the doubled DUT1 ticks (DUT1 is read from the code),
//! and two-digit years outside 2000–2099.

use num_complex::{Complex32, Complex64};
use scannerd_dsp::DecimFir;
use std::collections::VecDeque;
use std::f64::consts::TAU;

/// The rate the decoder works at, roughly: the input is cut to it by a
/// whole factor.
const WORK_RATE: f64 = 4_000.0;
/// One cycle of the 100 Hz subcarrier: the demodulation block.
const BLOCK_S: f64 = 0.010;
const TICK_S: f64 = 0.005;
/// What follows a tick is looked at this long for a steady tone (the
/// minute and hour tones), which at 100 % reads about 0.5 here, and the
/// level above which one is taken to be there.
const TONE_S: f64 = 0.025;
const TONE_GATE: f64 = 0.15;
/// Smoothing per sample of the tone measure's usual level (10 s).
const TONE_ALPHA: f64 = 1.0 / (10.0 * WORK_RATE);
const SUBCARRIER_HZ: f64 = 100.0;
/// Blocks on each side of the one demodulated whose carrier is averaged
/// for its reference: 50 ms in all, short against HF fading.
const CARRIER_SPAN: usize = 2;
/// The carrier may sit this far from the center.
const MAX_OFFSET_HZ: f64 = 150.0;
/// Bins of the code fold (5 ms) and the tick folds (0.5 ms).
const CODE_BINS: usize = 200;
const TICK_BINS: usize = 2000;
/// Each second the folds keep this much of what they held: about ten
/// seconds of memory.
const FOLD_DECAY: f32 = 0.9;
/// How far a fold's mean lags its newest second, in seconds.
const FOLD_LAG_S: f64 = FOLD_DECAY as f64 / (1.0 - FOLD_DECAY as f64) + 0.5;
/// Blocks kept, so the seconds heard before the second's start was found
/// can be read once it is.
const HISTORY_S: f64 = 70.0;
/// NIST's subcarrier levels, where the level estimates start.
const HIGH_NOMINAL: f32 = 0.18;
const LOW_NOMINAL: f32 = 0.03;
/// The windows each second is judged by, in ms after its start: where
/// every pulse is high, where 1s and markers are, where only markers are,
/// and where nothing is. Each keeps 15 ms clear of the edges.
const SEG_A: (f64, f64) = (45.0, 185.0);
const SEG_B: (f64, f64) = (215.0, 485.0);
const SEG_C: (f64, f64) = (515.0, 785.0);
const SEG_D: (f64, f64) = (815.0, 975.0);
/// The code edge's strength, in noise standard deviations, to take the
/// second's start and to keep it. The best of 200 noise phases reaches
/// about 3.
const EPOCH_TAKE: f32 = 7.0;
const EPOCH_HOLD: f32 = 4.0;
/// Seconds of a weak edge before the second's start is given up.
const EPOCH_LOSE_S: u32 = 30;
/// A measured start this far (in seconds) from the one held is a jump,
/// taken only after it has been seen for `EPOCH_MOVE_S` seconds running.
const EPOCH_JUMP: f64 = 0.03;
const EPOCH_MOVE_S: u32 = 3;
/// Share of the measured start error corrected each second.
const EPOCH_GAIN: f64 = 0.3;
/// The minute alignment must beat the next best by this much (nats).
const LOCK_MARGIN: f32 = 12.0;
/// A station's tick fold peak over its median, as a ratio of the excess
/// to the median, to be heard and to stay heard.
const TICK_TAKE: f32 = 1.0;
const TICK_HOLD: f32 = 0.5;
/// A station's peak must exceed the other filter's response there by
/// this factor: a tick leaking into the other filter, or a tone onset into
/// both, comes through at most about half as strong.
const TICK_DOMINANCE: f32 = 4.0;
/// An unused bit read as 1 by this much (nats) means the frame is wrong.
const UNUSED_REJECT: f32 = 4.0;
/// A minute is reported at this confidence or more.
const MIN_CONFIDENCE: f32 = 0.99;
/// Minutes of frames read together.
const WINDOW: usize = 10;
/// The newest frame may disagree with the reading on a bit by at most this
/// (nats), so a minute that faded out is not reported from the ones
/// before it.
const CONTRADICT: f32 = 4.0;
/// Soft bits are capped here, so no one second is beyond doubt.
const LLR_MAX: f32 = 30.0;
/// The sample clock is assumed within this of the station's (as a
/// fraction).
const MAX_DRIFT: f64 = 5e-4;
/// Smoothing per 10 ms block: the subcarrier's phase (2 s), the noise
/// (10 s) and the carrier's power (1 s).
const PHASE_ALPHA: f64 = 1.0 / 200.0;
const NOISE_ALPHA: f64 = 1.0 / 1000.0;
const POWER_ALPHA: f64 = 1.0 / 100.0;

/// Symbol classes, in the order of the log-likelihoods.
const HOLE: usize = 0;
const ZERO: usize = 1;
const ONE: usize = 2;
const MARK: usize = 3;

/// Values of [`TimecodeStatus::bits`].
pub const BIT_ZERO: u8 = 0;
pub const BIT_ONE: u8 = 1;
pub const BIT_MARKER: u8 = 2;
/// Second 0: no pulse.
pub const BIT_HOLE: u8 = 3;

#[derive(Clone, Copy, PartialEq)]
enum Slot {
    Hole,
    Marker,
    Unused,
    Data,
}

const fn slot(pos: usize) -> Slot {
    match pos {
        0 => Slot::Hole,
        9 | 19 | 29 | 39 | 49 | 59 => Slot::Marker,
        1 | 8 | 14 | 18 | 24 | 27 | 28 | 34 | 42..=48 => Slot::Unused,
        _ => Slot::Data,
    }
}

/// The bits of the year, day, hour and minute.
const TIME_BITS: [usize; 31] = [
    4, 5, 6, 7, 10, 11, 12, 13, 15, 16, 17, 20, 21, 22, 23, 25, 26, 30, 31, 32, 33, 35, 36, 37, 38, 40, 41, 51, 52, 53, 54,
];
/// The bits that change only at 00:00 UTC: DST1, the leap warning, DUT1's
/// sign, DST2, DUT1's magnitude.
const STEADY_BITS: [usize; 7] = [2, 3, 50, 55, 56, 57, 58];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Station {
    /// Fort Collins: 1000 Hz ticks.
    Wwv,
    /// Kauai: 1200 Hz ticks.
    Wwvh,
}

impl Station {
    pub fn name(self) -> &'static str {
        match self {
            Station::Wwv => "WWV",
            Station::Wwvh => "WWVH",
        }
    }

    pub fn tick_hz(self) -> f32 {
        match self {
            Station::Wwv => 1000.0,
            Station::Wwvh => 1200.0,
        }
    }

    const ALL: [Station; 2] = [Station::Wwv, Station::Wwvh];
}

/// Daylight saving time in the United States, from the code's two bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dst {
    Standard,
    Daylight,
    /// DST2 has changed to 1 at 00:00 UTC today; clocks go forward at 2 a.m. local.
    BeginsToday,
    /// DST2 has changed to 0 at 00:00 UTC today; clocks go back at 2 a.m. local.
    EndsToday,
}

impl Dst {
    /// DST2 (second 55) changes at 00:00 UTC on the day of the change,
    /// DST1 (second 2) 24 hours later.
    pub fn from_bits(dst1: bool, dst2: bool) -> Self {
        match (dst1, dst2) {
            (false, false) => Dst::Standard,
            (true, true) => Dst::Daylight,
            (false, true) => Dst::BeginsToday,
            (true, false) => Dst::EndsToday,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Dst::Standard => "standard time",
            Dst::Daylight => "daylight time",
            Dst::BeginsToday => "daylight time begins today",
            Dst::EndsToday => "daylight time ends today",
        }
    }
}

/// A UTC minute as the code gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UtcMinute {
    /// 2000 plus the code's two digits.
    pub year: u16,
    /// 1 to 366.
    pub day_of_year: u16,
    pub hour: u8,
    pub minute: u8,
}

impl UtcMinute {
    pub fn unix_s(&self) -> i64 {
        let days = days_from_civil(i64::from(self.year), 1, 1) + i64::from(self.day_of_year) - 1;
        days * 86_400 + i64::from(self.hour) * 3600 + i64::from(self.minute) * 60
    }

    /// The minute containing `s`.
    pub fn from_unix_s(s: i64) -> Self {
        let days = s.div_euclid(86_400);
        let rem = s.rem_euclid(86_400);
        let (y, _, _) = civil_from_days(days);
        UtcMinute {
            year: y as u16,
            day_of_year: (days - days_from_civil(y, 1, 1) + 1) as u16,
            hour: (rem / 3600) as u8,
            minute: (rem % 3600 / 60) as u8,
        }
    }

    pub fn add_minutes(&self, m: i64) -> Self {
        Self::from_unix_s(self.unix_s() + 60 * m)
    }

    /// Month (1–12) and day of the month.
    pub fn month_day(&self) -> (u8, u8) {
        let days = days_from_civil(i64::from(self.year), 1, 1) + i64::from(self.day_of_year) - 1;
        let (_, m, d) = civil_from_days(days);
        (m as u8, d as u8)
    }

    /// For example "2026-01-01T00:00Z".
    pub fn iso(&self) -> String {
        let (m, d) = self.month_day();
        format!("{:04}-{m:02}-{d:02}T{:02}:{:02}Z", self.year, self.hour, self.minute)
    }
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's method).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// One decoded minute.
#[derive(Clone, Debug, PartialEq)]
pub struct MinuteFrame {
    /// The UTC at the start of the frame.
    pub utc: UtcMinute,
    /// UT1 − UTC, −0.7 to +0.7 s.
    pub dut1_s: f32,
    pub dst1: bool,
    pub dst2: bool,
    pub dst: Dst,
    /// A leap second is to be added at the end of this month.
    pub leap_second_warning: bool,
    /// Probability that every field is right, from the soft bits of this
    /// minute and the ones before it (a bound on the chance of error).
    pub confidence: f32,
    /// Probability that this minute's own bits are all as read.
    pub frame_confidence: f32,
    /// Minutes of frames the reading was made from, this one included.
    pub minutes_used: u8,
    /// The stronger station heard by its ticks, if any.
    pub station: Option<Station>,
    pub snr_db: Option<f32>,
    /// The stronger station's tick arrival after the wall-clock second
    /// (−500 to +500 ms): propagation delay plus the local clock's error.
    pub tick_offset_ms: Option<f32>,
    /// The wall-clock time the frame's last on-time marker arrived, less
    /// the UTC it marks; whole seconds included.
    pub clock_offset_ms: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TimecodeEvent {
    Minute(MinuteFrame),
}

impl TimecodeEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            TimecodeEvent::Minute(_) => "minute",
        }
    }

    pub fn summary(&self) -> String {
        match self {
            TimecodeEvent::Minute(f) => {
                let (m, d) = f.utc.month_day();
                let mut s = format!(
                    "{:04}-{m:02}-{d:02} {:02}:{:02} UTC (day {:03}), DUT1 {:+.1} s, {}",
                    f.utc.year,
                    f.utc.hour,
                    f.utc.minute,
                    f.utc.day_of_year,
                    f.dut1_s,
                    f.dst.name()
                );
                if f.leap_second_warning {
                    s += ", leap second at month end";
                }
                if let Some(st) = f.station {
                    s += &format!(", {}", st.name());
                }
                if let Some(t) = f.tick_offset_ms {
                    s += &format!(", ticks {t:+.1} ms");
                }
                s += &format!(", confidence {:.1}% over {} min", 100.0 * f.confidence, f.minutes_used);
                s
            }
        }
    }

    pub fn fields(&self) -> Vec<(&'static str, String)> {
        match self {
            TimecodeEvent::Minute(f) => {
                let mut v = vec![
                    ("utc", f.utc.iso()),
                    ("year", f.utc.year.to_string()),
                    ("dayOfYear", f.utc.day_of_year.to_string()),
                    ("hour", f.utc.hour.to_string()),
                    ("minute", f.utc.minute.to_string()),
                    ("dut1", format!("{:+.1}", f.dut1_s)),
                    ("dst", f.dst.name().to_string()),
                    ("dst1", u8::from(f.dst1).to_string()),
                    ("dst2", u8::from(f.dst2).to_string()),
                    ("leapWarning", f.leap_second_warning.to_string()),
                    ("confidence", format!("{:.3}", f.confidence)),
                    ("frameConfidence", format!("{:.3}", f.frame_confidence)),
                    ("minutesUsed", f.minutes_used.to_string()),
                ];
                if let Some(st) = f.station {
                    v.push(("station", st.name().to_string()));
                }
                if let Some(s) = f.snr_db {
                    v.push(("snrDb", format!("{s:.1}")));
                }
                if let Some(t) = f.tick_offset_ms {
                    v.push(("tickOffsetMs", format!("{t:.2}")));
                }
                if let Some(c) = f.clock_offset_ms {
                    v.push(("clockOffsetMs", format!("{c:.2}")));
                }
                v
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TimecodeStatus {
    /// Aligned on the minute frame.
    pub locked: bool,
    /// The 100 Hz subcarrier's high level against the noise in a 100 Hz
    /// bandwidth around it; once the second's start has been found.
    pub snr_db: Option<f32>,
    /// Where the carrier is, relative to the center.
    pub carrier_offset_hz: Option<f32>,
    /// The stronger station heard by its ticks.
    pub station: Option<Station>,
    /// Each station's tick level over the noise, when it is heard.
    pub wwv_db: Option<f32>,
    pub wwvh_db: Option<f32>,
    /// The second of the minute now under way, when locked.
    pub second: Option<u8>,
    /// The current minute's symbols so far, by second: [`BIT_ZERO`],
    /// [`BIT_ONE`], [`BIT_MARKER`], [`BIT_HOLE`], or None not yet heard.
    pub bits: Vec<Option<u8>>,
    pub last_utc: Option<UtcMinute>,
    /// The stronger station's tick arrival after the wall-clock second.
    pub tick_offset_ms: Option<f32>,
    pub wwv_tick_ms: Option<f32>,
    pub wwvh_tick_ms: Option<f32>,
}

/// A 10 ms block waiting for the carrier after it.
struct RawBlock {
    start: u64,
    y: Vec<Complex32>,
    b: Complex32,
}

/// A demodulated block: the 100 Hz subcarrier's in-phase level, and the
/// block's weight (half its samples times the carrier's power), so `r / w`
/// is the modulation depth.
#[derive(Clone, Copy, Default)]
struct Block {
    r: f32,
    w: f32,
}

/// A tick matched-filter output waiting to be folded, and where.
#[derive(Clone, Copy, Default)]
struct Late {
    e: [f32; 2],
    phase: f64,
    nominal: f64,
    full: bool,
}

/// One station's tick folds: one turning with the station's second as the
/// drift is known, for when the ticks arrive, and one on the nominal local
/// second, for the drift itself. Measuring the drift from the first would
/// feed its own error back into it.
struct TickFold {
    f: Vec<f32>,
    w: Vec<f32>,
    fn_: Vec<f32>,
    wn: Vec<f32>,
    present: bool,
    snr: f32,
    /// The nominal fold's peak, as a fraction of the local second.
    phase: f64,
    offset_ms: Option<f32>,
}

impl TickFold {
    fn new() -> Self {
        Self {
            f: vec![0.0; TICK_BINS],
            w: vec![0.0; TICK_BINS],
            fn_: vec![0.0; TICK_BINS],
            wn: vec![0.0; TICK_BINS],
            present: false,
            snr: 0.0,
            phase: 0.0,
            offset_ms: None,
        }
    }
}

/// The peak of a fold profile over its floor, interpolated by the centroid
/// of the 8 ms around it, as a fraction of the fold.
fn fold_peak(p: &[f32], floor: f32) -> (usize, f64) {
    let n = p.len();
    let mut ipk = 0;
    for i in 1..n {
        if p[i] > p[ipk] {
            ipk = i;
        }
    }
    let (mut num, mut den) = (0.0f64, 0.0f64);
    for k in -8i32..=8 {
        let i = (ipk as i32 + k).rem_euclid(n as i32) as usize;
        let v = f64::from((p[i] - floor).max(0.0));
        num += v * f64::from(k);
        den += v;
    }
    (ipk, ((ipk as f64 + num / den.max(1e-30)) / n as f64).rem_euclid(1.0))
}

/// One second's log-likelihoods for each class.
#[derive(Clone, Copy)]
struct Sym {
    j: u64,
    ll: [f32; 4],
    /// Working-sample index of the second's start.
    start: f64,
}

impl Sym {
    fn class(&self) -> usize {
        let mut best = 0;
        for c in 1..4 {
            if self.ll[c] > self.ll[best] {
                best = c;
            }
        }
        best
    }
}

/// The second's start, as found from the code.
struct Epoch {
    /// The next second to judge, and where it starts.
    next_j: u64,
    next_start: f64,
    weak: u32,
    far: u32,
}

pub struct TimecodeDecoder {
    fs: f64,
    d: usize,
    /// Input samples from a decimator output's window start to its center.
    fir_center: f64,
    fw: f64,
    nb: usize,
    sub: usize,
    k: usize,
    fir: DecimFir,
    scratch: Vec<Complex32>,
    n_in: u64,
    n: u64,
    // Carrier.
    nco: Complex64,
    nco_step: Complex64,
    f_hat: f64,
    sub_acc: Complex32,
    sub_cnt: usize,
    prev_sub: Option<Complex32>,
    fll: Complex32,
    fll_blocks: u64,
    // Blocks.
    cur: Vec<Complex32>,
    cur_b: Complex32,
    cur_start: u64,
    raw: VecDeque<RawBlock>,
    spare: Vec<Vec<Complex32>>,
    ring: Vec<Block>,
    ring_lo: u64,
    ring_hi: u64,
    ph100: Complex64,
    nu_num: f64,
    nu_den: f64,
    pbar: f64,
    // Folds.
    code_f: Vec<f32>,
    code_w: Vec<f32>,
    ticks: [TickFold; 2],
    tick_hist: Vec<[Complex32; 4]>,
    tick_pos: usize,
    tick_sum: [Complex64; 4],
    tick_rot: [Complex64; 4],
    tick_step: [Complex64; 4],
    tone_hist: Vec<[Complex32; 4]>,
    tone_pos: usize,
    tone_sum: [Complex64; 4],
    tone_delay: Vec<Late>,
    tone_mean: [f64; 2],
    sec: u64,
    /// Where the next sample falls in the tick folds, as a fraction of the
    /// station's second.
    tick_phase: f64,
    // Timing.
    epoch: Option<Epoch>,
    j_base: u64,
    code_phase: f64,
    code_q: f32,
    drift: f64,
    drift_hist: VecDeque<(f64, f64)>,
    drift_src: usize,
    // Symbols and frames.
    high: f32,
    low: f32,
    syms: VecDeque<Sym>,
    frame_phase: Option<usize>,
    window: VecDeque<Soft>,
    last_utc: Option<UtcMinute>,
    snr_db: Option<f32>,
    /// The in-phase residuals' power against the weight, as `nu_num` /
    /// `nu_den` are for the quadrature.
    res_num: f64,
    res_den: f64,
    /// Input sample index and the wall-clock time (ms) it was taken at.
    time_ref: Option<(f64, f64)>,
    events: Vec<TimecodeEvent>,
}

/// To the nearest whole, as a fraction in [−0.5, 0.5].
fn wrap(x: f64) -> f64 {
    x - x.round()
}

impl TimecodeDecoder {
    pub fn new(fs: f32) -> Self {
        let fs = f64::from(fs);
        let d = ((fs / WORK_RATE).round() as usize).max(1);
        let fw = fs / d as f64;
        // About 1.1 kHz of transition from the 1.45 kHz the ticks reach to
        // where aliases would land on them; well within the direct path.
        let ntaps = (15 * d).max(15) | 1;
        let fir = DecimFir::new((fw / 2.0) as f32, fs as f32, d, ntaps);
        let nb = (fw * BLOCK_S).round() as usize;
        let k = (fw * TICK_S).round() as usize;
        let k2 = (fw * TONE_S).round() as usize;
        let cap = (HISTORY_S / BLOCK_S) as usize;
        let mut tick_step = [Complex64::new(1.0, 0.0); 4];
        for (i, s) in tick_step.iter_mut().enumerate() {
            let sign = if i % 2 == 0 { -1.0 } else { 1.0 };
            *s = Complex64::from_polar(1.0, sign * TAU * f64::from(Station::ALL[i / 2].tick_hz()) / fw);
        }
        Self {
            fs,
            d,
            fir_center: (ntaps - 1) as f64 / 2.0,
            fw,
            nb,
            sub: (nb / 4).max(1),
            k,
            fir,
            scratch: Vec::new(),
            n_in: 0,
            n: 0,
            nco: Complex64::new(1.0, 0.0),
            nco_step: Complex64::new(1.0, 0.0),
            f_hat: 0.0,
            sub_acc: Complex32::default(),
            sub_cnt: 0,
            prev_sub: None,
            fll: Complex32::default(),
            fll_blocks: 0,
            cur: Vec::with_capacity(nb),
            cur_b: Complex32::default(),
            cur_start: 0,
            raw: VecDeque::with_capacity(2 * CARRIER_SPAN + 2),
            spare: Vec::new(),
            ring: vec![Block::default(); cap],
            ring_lo: 0,
            ring_hi: 0,
            ph100: Complex64::default(),
            nu_num: 0.0,
            nu_den: 0.0,
            pbar: 0.0,
            code_f: vec![0.0; CODE_BINS],
            code_w: vec![0.0; CODE_BINS],
            ticks: [TickFold::new(), TickFold::new()],
            tick_hist: vec![[Complex32::default(); 4]; k],
            tick_pos: 0,
            tick_sum: [Complex64::default(); 4],
            tick_rot: [Complex64::new(1.0, 0.0); 4],
            tick_step,
            tone_hist: vec![[Complex32::default(); 4]; k2],
            tone_pos: 0,
            tone_sum: [Complex64::default(); 4],
            tone_delay: vec![Late::default(); k2],
            tone_mean: [0.0; 2],
            sec: 0,
            tick_phase: 0.0,
            epoch: None,
            j_base: 0,
            code_phase: 0.0,
            code_q: 0.0,
            drift: 0.0,
            drift_hist: VecDeque::new(),
            drift_src: usize::MAX,
            high: HIGH_NOMINAL,
            low: LOW_NOMINAL,
            syms: VecDeque::with_capacity(61),
            frame_phase: None,
            window: VecDeque::with_capacity(WINDOW + 1),
            last_utc: None,
            snr_db: None,
            res_num: 0.0,
            res_den: 0.0,
            time_ref: None,
            events: Vec::new(),
        }
    }

    /// I/Q with no wall-clock time: tick offsets are not reported.
    pub fn push_iq(&mut self, iq: &[Complex32]) {
        self.time_ref = None;
        self.process(iq);
    }

    /// I/Q whose first sample was taken at `t0_unix_ms` by the wall clock.
    pub fn push_iq_at(&mut self, iq: &[Complex32], t0_unix_ms: f64) {
        let i = self.n_in as f64;
        // Timestamps jitter by however the chunk was stamped; the sample
        // count does not. A small correction each chunk follows the clock
        // without the jitter, and a jump restarts it.
        let ms = match self.wall_ms(i) {
            Some(p) if (t0_unix_ms - p).abs() < 50.0 => p + 0.1 * (t0_unix_ms - p),
            _ => t0_unix_ms,
        };
        self.time_ref = Some((i, ms));
        self.process(iq);
    }

    pub fn take(&mut self) -> Vec<TimecodeEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn status(&self) -> TimecodeStatus {
        let mut bits = vec![None; 60];
        let mut second = None;
        if let (Some(ph), Some(last)) = (self.frame_phase, self.syms.back()) {
            let pos_last = Self::pos(last.j, ph);
            second = Some(((pos_last + 1) % 60) as u8);
            let first = last.j - pos_last as u64;
            for s in self.syms.iter().filter(|s| s.j >= first) {
                bits[Self::pos(s.j, ph)] = Some(match s.class() {
                    HOLE => BIT_HOLE,
                    ZERO => BIT_ZERO,
                    ONE => BIT_ONE,
                    _ => BIT_MARKER,
                });
            }
        }
        let level = |s: usize| self.ticks[s].present.then(|| 10.0 * self.ticks[s].snr.log10());
        let station = self.station();
        let heard = self.epoch.is_some() || station.is_some();
        TimecodeStatus {
            locked: self.frame_phase.is_some(),
            snr_db: self.snr_db,
            carrier_offset_hz: heard.then_some(self.f_hat as f32),
            station,
            wwv_db: level(0),
            wwvh_db: level(1),
            second,
            bits,
            last_utc: self.last_utc,
            tick_offset_ms: station.and_then(|s| self.ticks[s as usize].offset_ms),
            wwv_tick_ms: self.ticks[0].offset_ms.filter(|_| self.ticks[0].present),
            wwvh_tick_ms: self.ticks[1].offset_ms.filter(|_| self.ticks[1].present),
        }
    }

    fn station(&self) -> Option<Station> {
        let mut best: Option<usize> = None;
        for s in 0..2 {
            if self.ticks[s].present && best.is_none_or(|b| self.ticks[s].snr > self.ticks[b].snr) {
                best = Some(s);
            }
        }
        best.map(|s| Station::ALL[s])
    }

    /// The wall-clock time of an input sample, counted from the last
    /// timestamp at the sample rate the station's seconds say it is.
    fn wall_ms(&self, input_index: f64) -> Option<f64> {
        self.time_ref.map(|(i, ms)| ms + (input_index - i) * 1000.0 / (self.fs * (1.0 + self.drift)))
    }

    /// The input sample index a (fractional) working sample stands for.
    fn input_index(&self, n: f64) -> f64 {
        n * self.d as f64 + self.fir_center
    }

    /// Working samples in one of the station's seconds.
    fn sec_len(&self) -> f64 {
        self.fw * (1.0 + self.drift)
    }

    fn pos(j: u64, phase: usize) -> usize {
        ((j % 60) as usize + 60 - phase) % 60
    }

    fn process(&mut self, iq: &[Complex32]) {
        let mut out = std::mem::take(&mut self.scratch);
        self.fir.process(iq, &mut out);
        for &z in &out {
            self.work(z);
        }
        self.scratch = out;
        self.n_in += iq.len() as u64;
        self.catch_up();
    }

    /// One working sample.
    fn work(&mut self, z: Complex32) {
        let y = z * Complex32::new(self.nco.re as f32, self.nco.im as f32);
        self.nco *= self.nco_step;

        // The frequency-locked loop's error: the carrier's phase advance
        // from one 2.5 ms sub-block to the next, unambiguous to ±200 Hz.
        // The AM's sidebands are symmetric and add nothing to it.
        self.sub_acc += y;
        self.sub_cnt += 1;
        if self.sub_cnt == self.sub {
            if let Some(p) = self.prev_sub {
                self.fll += self.sub_acc * p.conj();
            }
            self.prev_sub = Some(self.sub_acc);
            self.sub_acc = Complex32::default();
            self.sub_cnt = 0;
        }

        // Tick matched filters: 5 ms at each station's tone, each sideband
        // apart.
        let old = self.tick_hist[self.tick_pos];
        let mut new = [Complex32::default(); 4];
        for i in 0..4 {
            let r = self.tick_rot[i];
            new[i] = y * Complex32::new(r.re as f32, r.im as f32);
            self.tick_rot[i] *= self.tick_step[i];
            self.tick_sum[i] += Complex64::new(f64::from(new[i].re - old[i].re), f64::from(new[i].im - old[i].im));
        }
        self.tick_hist[self.tick_pos] = new;
        self.tick_pos = (self.tick_pos + 1) % self.k;
        // The same over 25 ms: a tone, not a tick.
        let old = self.tone_hist[self.tone_pos];
        for i in 0..4 {
            self.tone_sum[i] += Complex64::new(f64::from(new[i].re - old[i].re), f64::from(new[i].im - old[i].im));
        }
        self.tone_hist[self.tone_pos] = new;
        if self.pbar > 0.0 {
            let k2 = self.tone_hist.len() as f64;
            let (norm, norm2) = (1.0 / (self.pbar * (self.k * self.k) as f64), 1.0 / (self.pbar * k2 * k2));
            // Each matched-filter output goes into the folds 25 ms late, and
            // only if those 25 ms after it hold no steady tone at its
            // frequency: the minute tone starts like a tick, and its
            // plateau would otherwise pull the tick's position late.
            // The gate stands that far above the measure's own usual
            // level, which in noise alone is far above the tone's.
            let late = self.tone_delay[self.tone_pos];
            for s in 0..2 {
                let e = ((self.tick_sum[2 * s].norm_sqr() + self.tick_sum[2 * s + 1].norm_sqr()) * norm) as f32;
                let tone = (self.tone_sum[2 * s].norm_sqr() + self.tone_sum[2 * s + 1].norm_sqr()) * norm2;
                self.tone_mean[s] += (tone - self.tone_mean[s]) * TONE_ALPHA;
                if late.full && tone < TONE_GATE + 3.0 * self.tone_mean[s] {
                    let t = &mut self.ticks[s];
                    deposit(&mut t.f, &mut t.w, late.phase, late.e[s], 1.0);
                    deposit(&mut t.fn_, &mut t.wn, late.nominal, late.e[s], 1.0);
                }
                self.tone_delay[self.tone_pos].e[s] = e;
            }
        }
        self.tone_delay[self.tone_pos].phase = self.tick_phase;
        self.tone_delay[self.tone_pos].nominal = (self.n as f64 / self.fw).fract();
        self.tone_delay[self.tone_pos].full = self.pbar > 0.0;
        self.tone_pos = (self.tone_pos + 1) % self.tone_hist.len();
        // The tick folds turn with the station's second, as far as the
        // drift is known, so a tick stays put in them instead of smearing.
        self.tick_phase = (self.tick_phase + 1.0 / self.sec_len()).fract();

        self.cur.push(y);
        self.cur_b += y;
        if self.cur.len() == self.nb {
            self.finish_block();
        }
        self.n += 1;
        let s = (self.n as f64 / self.fw) as u64;
        if s > self.sec {
            self.sec = s;
            self.second();
        }
    }

    fn finish_block(&mut self) {
        let fresh = self.spare.pop().unwrap_or_else(|| Vec::with_capacity(self.nb));
        let y = std::mem::replace(&mut self.cur, fresh);
        self.raw.push_back(RawBlock { start: self.cur_start, y, b: self.cur_b });
        self.cur_b = Complex32::default();
        self.cur_start += self.nb as u64;

        if self.fll.norm_sqr() > 0.0 {
            let err_hz = f64::from(self.fll.arg()) * self.fw / (TAU * self.sub as f64);
            // Fast to pull in, then slow enough to sit still in noise.
            let mu = if self.fll_blocks < 300 { 0.05 } else { 0.01 };
            self.f_hat = (self.f_hat + mu * err_hz).clamp(-MAX_OFFSET_HZ, MAX_OFFSET_HZ);
            self.nco_step = Complex64::from_polar(1.0, -TAU * self.f_hat / self.fw);
        }
        self.fll = Complex32::default();
        self.fll_blocks += 1;
        self.nco /= self.nco.norm();
        // The tick oscillators restart exactly on the sample count, so they
        // cannot wander from it.
        let t = self.cur_start as f64 / self.fw;
        for i in 0..4 {
            let sign = if i % 2 == 0 { -1.0 } else { 1.0 };
            let f = f64::from(Station::ALL[i / 2].tick_hz());
            self.tick_rot[i] = Complex64::from_polar(1.0, sign * TAU * (f * t).fract());
        }
        // And the running sums are taken afresh, so rounding cannot build.
        let resum = |hist: &[[Complex32; 4]]| {
            let mut sum = [Complex64::default(); 4];
            for h in hist {
                for i in 0..4 {
                    sum[i] += Complex64::new(f64::from(h[i].re), f64::from(h[i].im));
                }
            }
            sum
        };
        self.tick_sum = resum(&self.tick_hist);
        self.tone_sum = resum(&self.tone_hist);

        if self.raw.len() == 2 * CARRIER_SPAN + 1 {
            self.finalize();
            if let Some(mut rb) = self.raw.pop_front() {
                rb.y.clear();
                self.spare.push(rb.y);
            }
        }
    }

    /// Demodulate the middle block of those waiting, against the carrier
    /// averaged over all of them.
    fn finalize(&mut self) {
        let span = (2 * CARRIER_SPAN + 1) as f32;
        let c = self.raw.iter().map(|r| r.b).sum::<Complex32>() / (span * self.nb as f32);
        let blk = &self.raw[CARRIER_SPAN];
        let p = f64::from(c.norm_sqr());
        let mut block = Block::default();
        if p > 1e-30 {
            let ang0 = TAU * (SUBCARRIER_HZ * blk.start as f64 / self.fw).fract();
            let mut rot = Complex64::from_polar(1.0, -ang0);
            let step = Complex64::from_polar(1.0, -TAU * SUBCARRIER_HZ / self.fw);
            let cc = c.conj();
            let mut acc = Complex64::default();
            for &y in &blk.y {
                // The in-phase part less the carrier: the modulation.
                let d = f64::from((y * cc).re) - p;
                acc += rot * d;
                rot *= step;
            }
            let w = p * self.nb as f64 / 2.0;
            self.ph100 += (acc - self.ph100) * PHASE_ALPHA;
            let norm = self.ph100.norm();
            let u = if norm > 0.0 { self.ph100 / norm } else { Complex64::new(1.0, 0.0) };
            let v = acc * u.conj();
            self.nu_num += (v.im * v.im - self.nu_num) * NOISE_ALPHA;
            self.nu_den += (w - self.nu_den) * NOISE_ALPHA;
            self.pbar = if self.pbar == 0.0 { p } else { self.pbar + (p - self.pbar) * POWER_ALPHA };
            block = Block { r: v.re as f32, w: w as f32 };
        }
        let idx = blk.start / self.nb as u64;
        let center = blk.start as f64 + (self.nb as f64 - 1.0) / 2.0;
        let cap = self.ring.len() as u64;
        if self.ring_hi == 0 {
            self.ring_lo = idx;
        }
        self.ring[(idx % cap) as usize] = block;
        self.ring_hi = idx + 1;
        self.ring_lo = self.ring_lo.max(self.ring_hi.saturating_sub(cap));
        let ph = (center / self.fw).fract();
        deposit(&mut self.code_f, &mut self.code_w, ph, block.r, block.w);
    }

    /// Once a local second: where the second starts, the ticks, the drift.
    fn second(&mut self) {
        self.eval_epoch();
        self.eval_ticks();
        self.eval_drift();
        for v in self.code_f.iter_mut().chain(self.code_w.iter_mut()) {
            *v *= FOLD_DECAY;
        }
        for t in &mut self.ticks {
            for v in t.f.iter_mut().chain(t.w.iter_mut()).chain(t.fn_.iter_mut()).chain(t.wn.iter_mut()) {
                *v *= FOLD_DECAY;
            }
        }
    }

    /// The noise in the 100 Hz detector, per unit weight: from its
    /// quadrature part, or from the in-phase residuals where nothing is
    /// sent, whichever is larger. The second holds what the first misses
    /// (leakage of the tones, the code's own imperfections); the first is
    /// there from the start.
    fn noise(&self) -> Option<f64> {
        let q = (self.nu_den > 0.0 && self.nu_num > 0.0).then(|| self.nu_num / self.nu_den)?;
        let r = if self.res_den > 0.5 { self.res_num / self.res_den } else { 0.0 };
        Some(q.max(r))
    }

    /// The second's start from the code fold: the rising edge out of the
    /// protected zone, as the mean depth from 40 to 190 ms less that from
    /// −10 to 25 ms, at each of the fold's 200 phases.
    fn eval_epoch(&mut self) {
        let Some(nu) = self.noise() else { return };
        let mut prof = [0f32; CODE_BINS];
        let mut inv = [0f64; CODE_BINS];
        for b in 0..CODE_BINS {
            if self.code_w[b] > 0.0 {
                prof[b] = self.code_f[b] / self.code_w[b];
                inv[b] = 1.0 / f64::from(self.code_w[b]);
            } else {
                inv[b] = 1e30;
            }
        }
        let at = |e: usize, k: isize| (e as isize + k).rem_euclid(CODE_BINS as isize) as usize;
        let score = |e: usize| -> (f32, f64) {
            let (mut hi, mut vhi) = (0.0f32, 0.0f64);
            for k in 8..38 {
                hi += prof[at(e, k)];
                vhi += inv[at(e, k)];
            }
            let (mut lo, mut vlo) = (0.0f32, 0.0f64);
            for k in -2..5 {
                lo += prof[at(e, k)];
                vlo += inv[at(e, k)];
            }
            (hi / 30.0 - lo / 7.0, vhi / 900.0 + vlo / 49.0)
        };
        let mut s = [0f32; CODE_BINS];
        for (e, v) in s.iter_mut().enumerate() {
            *v = score(e).0;
        }
        let mut best = 0;
        for e in 1..CODE_BINS {
            if s[e] > s[best] {
                best = e;
            }
        }
        // A fold bin's variance: the noise over its weight, and less by
        // 1 / (1 + decay) for the exponential memory.
        let var = nu * score(best).1 / (1.0 + f64::from(FOLD_DECAY));
        let q = (f64::from(s[best]) / var.sqrt()) as f32;
        let (sm, sp) = (s[at(best, -1)], s[at(best, 1)]);
        let den = sm - 2.0 * s[best] + sp;
        let delta = if den < 0.0 { (0.5 * (sm - sp) / den).clamp(-0.5, 0.5) } else { 0.0 };
        let phase = ((best as f32 + delta) / CODE_BINS as f32) as f64;
        self.code_q = q;
        self.code_phase = (phase + self.drift * FOLD_LAG_S).rem_euclid(1.0);

        let now = self.n as f64;
        let phase = self.code_phase;
        let ls = self.sec_len();
        let fw = self.fw;
        let mut restart = false;
        let mut lose = false;
        match &mut self.epoch {
            None => restart = q >= EPOCH_TAKE,
            Some(ep) => {
                if q < EPOCH_HOLD {
                    ep.weak += 1;
                    lose = ep.weak >= EPOCH_LOSE_S;
                } else {
                    ep.weak = 0;
                    let k = ((now - ep.next_start) / ls).round();
                    let near = ep.next_start + k * ls;
                    let d = wrap(phase - (near / fw).rem_euclid(1.0));
                    if d.abs() < EPOCH_JUMP {
                        ep.next_start += EPOCH_GAIN * d * fw;
                        ep.far = 0;
                    } else if q >= EPOCH_TAKE {
                        ep.far += 1;
                        restart = ep.far >= EPOCH_MOVE_S;
                    }
                }
            }
        }
        if lose {
            self.epoch = None;
            self.reset_frames();
        } else if restart {
            self.start_epoch(phase);
        }
        self.snr_db = match (&self.epoch, self.noise()) {
            (Some(_), Some(nu)) => {
                let w = self.pbar * self.nb as f64 / 2.0;
                let snr = f64::from(self.high).powi(2) * w / (2.0 * nu);
                Some((10.0 * snr.log10()) as f32)
            }
            _ => None,
        };
    }

    fn reset_frames(&mut self) {
        self.syms.clear();
        self.frame_phase = None;
        self.window.clear();
    }

    /// The second's start is found (or has moved): judge every second the
    /// kept blocks cover again from there.
    fn start_epoch(&mut self, phase: f64) {
        let now = self.n as f64;
        let ls = self.sec_len();
        let mut a = ((now / self.fw).floor() + phase) * self.fw;
        if a > now {
            a -= self.fw;
        }
        let oldest = (self.ring_lo * self.nb as u64) as f64;
        let back = ((a - oldest) / ls).floor().clamp(0.0, 65.0);
        self.j_base += 1000;
        self.epoch = Some(Epoch { next_j: self.j_base, next_start: a - back * ls, weak: 0, far: 0 });
        self.reset_frames();
        self.catch_up();
    }

    /// Judge every second whose blocks are all in.
    fn catch_up(&mut self) {
        loop {
            let ls = self.sec_len();
            let Some(ep) = &self.epoch else { return };
            let (j, start) = (ep.next_j, ep.next_start);
            if start + ls > (self.ring_hi * self.nb as u64) as f64 {
                return;
            }
            self.classify(j, start, ls);
            if let Some(ep) = &mut self.epoch {
                ep.next_j += 1;
                ep.next_start += ls;
            }
        }
    }

    /// The weighted mean depth over `seg` (ms) of the second starting at
    /// `start`, and the weight behind it.
    fn segment(&self, start: f64, ls: f64, seg: (f64, f64)) -> (f32, f64) {
        let nb = self.nb as f64;
        let s0 = start + seg.0 / 1000.0 * ls;
        let s1 = start + seg.1 / 1000.0 * ls;
        let b0 = ((s0 / nb).ceil().max(0.0) as u64).max(self.ring_lo);
        let b1 = ((s1 / nb).floor().max(0.0) as u64).min(self.ring_hi);
        let cap = self.ring.len() as u64;
        let (mut r, mut w) = (0.0f64, 0.0f64);
        for b in b0..b1 {
            let blk = self.ring[(b % cap) as usize];
            r += f64::from(blk.r);
            w += f64::from(blk.w);
        }
        if w > 0.0 { ((r / w) as f32, w) } else { (0.0, 0.0) }
    }

    fn classify(&mut self, j: u64, start: f64, ls: f64) {
        let nu = self.noise().unwrap_or(1.0);
        let (h, l) = (self.high, self.low);
        let floor = (0.05 * (h - l)).powi(2);
        let segs = [self.segment(start, ls, SEG_A), self.segment(start, ls, SEG_B), self.segment(start, ls, SEG_C)];
        // Expected levels in the three windows for each class.
        let mu = [[l, l, l], [h, l, l], [h, h, l], [h, h, h]];
        let mut ll = [0f32; 4];
        for (c, m) in mu.iter().enumerate() {
            for (k, &(x, w)) in segs.iter().enumerate() {
                if w > 0.0 {
                    let v = (nu / w) as f32 + floor;
                    ll[c] -= (x - m[k]).powi(2) / (2.0 * v);
                }
            }
        }
        let sym = Sym { j, ll, start };

        // The levels follow the station: the high level where every pulse
        // is up, the low where none is, from seconds heard well enough.
        let full = self.pbar * self.nb as f64 / 2.0 * 14.0;
        let (xa, wa) = segs[0];
        if wa > 0.3 * full && sym.class() != HOLE {
            self.high = (h + 0.03 * (xa - h)).clamp(0.04, 0.6);
        }
        let (xd, wd) = self.segment(start, ls, SEG_D);
        if wd > 0.3 * full {
            self.low = (l + 0.03 * (xd - l)).clamp(0.0, 0.6 * self.high);
            self.res_num += 0.02 * (f64::from(xd - l).powi(2) * wd - self.res_num);
            self.res_den += 0.02 * (1.0 - self.res_den);
        }

        self.syms.push_back(sym);
        while self.syms.len() > 60 {
            self.syms.pop_front();
        }
        self.frame_sync();
        if let Some(ph) = self.frame_phase
            && Self::pos(j, ph) == 59
            && self.syms.len() == 60
        {
            self.frame();
        }
    }

    /// Which second of the last minute is second 0.
    fn frame_sync(&mut self) {
        if self.syms.len() < 60 {
            return;
        }
        let mut score = [0f32; 60];
        for (ph, sc) in score.iter_mut().enumerate() {
            for s in &self.syms {
                let max = s.ll.iter().copied().fold(f32::MIN, f32::max);
                let fit = match slot(Self::pos(s.j, ph)) {
                    Slot::Hole => s.ll[HOLE],
                    Slot::Marker => s.ll[MARK],
                    Slot::Unused => s.ll[ZERO],
                    Slot::Data => s.ll[ZERO].max(s.ll[ONE]),
                };
                *sc += fit - max;
            }
        }
        let mut best = 0;
        for ph in 1..60 {
            if score[ph] > score[best] {
                best = ph;
            }
        }
        let second = (0..60).filter(|&p| p != best).map(|p| score[p]).fold(f32::MIN, f32::max);
        match self.frame_phase {
            None if score[best] - second >= LOCK_MARGIN => self.frame_phase = Some(best),
            Some(cur) if cur != best && score[best] - score[cur] >= LOCK_MARGIN => {
                self.frame_phase = Some(best);
                self.window.clear();
            }
            _ => {}
        }
    }

    /// Read the minute that has just ended, with the minutes before it.
    fn frame(&mut self) {
        let syms: Vec<Sym> = self.syms.iter().copied().collect();
        let j_end = syms[59].j;
        if !structure_ok(&syms) {
            return;
        }
        let mut llr = [0f32; 60];
        for (l, s) in llr.iter_mut().zip(&syms) {
            *l = (s.ll[ONE] - s.ll[ZERO]).clamp(-LLR_MAX, LLR_MAX);
        }
        self.window.push_back(Soft { j_end, llr });
        while self.window.len() > WINDOW || self.window.front().is_some_and(|f| j_end - f.j_end >= 60 * WINDOW as u64) {
            self.window.pop_front();
        }
        let window: Vec<Soft> = self.window.iter().copied().collect();
        let Some(r) = combine(&window) else { return };
        if r.confidence < MIN_CONFIDENCE {
            return;
        }
        let utc = r.utc;
        let station = self.station();
        let tick = station.and_then(|s| self.ticks[s as usize].offset_ms);
        // When the frame's last second started by the wall clock, less the
        // UTC it is; the tick gives the fraction more finely than the code.
        let clock = self.wall_ms(self.input_index(syms[59].start)).map(|w| {
            let coarse = w - (utc.unix_s() + 59) as f64 * 1000.0;
            match tick {
                Some(t) => ((coarse - f64::from(t)) / 1000.0).round() * 1000.0 + f64::from(t),
                None => coarse,
            }
        });
        self.last_utc = Some(utc);
        self.events.push(TimecodeEvent::Minute(MinuteFrame {
            utc,
            dut1_s: r.dut1,
            dst1: r.dst1,
            dst2: r.dst2,
            dst: Dst::from_bits(r.dst1, r.dst2),
            leap_second_warning: r.leap,
            confidence: r.confidence,
            frame_confidence: r.frame_confidence,
            minutes_used: r.minutes,
            station,
            snr_db: self.snr_db,
            tick_offset_ms: tick,
            clock_offset_ms: clock,
        }));
    }

    /// Each station's tick fold: its peak against its median, whether it is
    /// that station's own, where it is, and when it arrived.
    fn eval_ticks(&mut self) {
        let profile = |f: &[f32], w: &[f32]| -> (Vec<f32>, f32) {
            let p: Vec<f32> = f.iter().zip(w).map(|(&f, &w)| if w > 0.0 { f / w } else { 0.0 }).collect();
            let mut sorted = p.clone();
            let mid = p.len() / 2;
            sorted.select_nth_unstable_by(mid, f32::total_cmp);
            (p, sorted[mid])
        };
        let folds = [profile(&self.ticks[0].f, &self.ticks[0].w), profile(&self.ticks[1].f, &self.ticks[1].w)];
        let now = self.n as f64;
        for s in 0..2 {
            let (p, fl) = (&folds[s].0, folds[s].1);
            let (ipk, peak) = fold_peak(p, fl);
            let ex = p[ipk] - fl;
            let snr = if fl > 0.0 { ex / fl } else { 0.0 };
            let other = (folds[1 - s].0[ipk] - folds[1 - s].1).max(0.0);
            let t = &self.ticks[s];
            let take = snr >= TICK_TAKE || (t.present && snr >= TICK_HOLD);
            let present = take && fl > 0.0 && ex > TICK_DOMINANCE * other;
            let mut phase = t.phase;
            let mut offset = None;
            if present {
                let (pn, fln) = profile(&t.fn_, &t.wn);
                phase = fold_peak(&pn, fln).1;
                // The most recent tick: the last sample whose fold phase is
                // the peak's. The matched filter's output there is centered
                // half a filter after the middle of the tick.
                let back = (self.tick_phase - peak).rem_euclid(1.0) * self.sec_len();
                let nt = now - back;
                let start = nt - self.k as f64 / 2.0 + 0.5 - TICK_S / 2.0 * self.fw;
                offset = self.wall_ms(self.input_index(start)).map(|w| {
                    let m = w.rem_euclid(1000.0);
                    (if m > 500.0 { m - 1000.0 } else { m }) as f32
                });
            }
            let t = &mut self.ticks[s];
            t.present = present;
            t.snr = snr;
            t.phase = phase;
            t.offset_ms = offset;
        }
    }

    /// The sample clock's drift against the station's: the slope of the
    /// strongest timing reference's position over the last half minute.
    fn eval_drift(&mut self) {
        let mut src = None;
        for s in 0..2 {
            if self.ticks[s].present && src.is_none_or(|b: usize| self.ticks[s].snr > self.ticks[b].snr) {
                src = Some(s);
            }
        }
        let (src, ph) = match src {
            Some(s) => (s, self.ticks[s].phase),
            None if self.epoch.is_some() && self.code_q >= EPOCH_TAKE => (2, self.code_phase - self.drift * FOLD_LAG_S),
            None => {
                self.drift_hist.clear();
                return;
            }
        };
        if src != self.drift_src {
            self.drift_hist.clear();
            self.drift_src = src;
        }
        let u = match self.drift_hist.back() {
            Some(&(_, last)) => last + wrap(ph - last),
            None => ph,
        };
        self.drift_hist.push_back((self.sec as f64, u));
        while self.drift_hist.len() > 30 {
            self.drift_hist.pop_front();
        }
        let n = self.drift_hist.len() as f64;
        if n >= 10.0 {
            let mt = self.drift_hist.iter().map(|p| p.0).sum::<f64>() / n;
            let mu = self.drift_hist.iter().map(|p| p.1).sum::<f64>() / n;
            let sxy: f64 = self.drift_hist.iter().map(|p| (p.0 - mt) * (p.1 - mu)).sum();
            let sxx: f64 = self.drift_hist.iter().map(|p| (p.0 - mt).powi(2)).sum();
            if sxx > 0.0 {
                self.drift = (sxy / sxx).clamp(-MAX_DRIFT, MAX_DRIFT);
            }
        }
    }
}

/// Add `v` (weight `w`) to a fold at `phase` (a fraction of the second),
/// split between the two nearest bins.
fn deposit(f: &mut [f32], wf: &mut [f32], phase: f64, v: f32, w: f32) {
    let n = f.len();
    let x = phase * n as f64;
    let i = (x as usize).min(n - 1);
    let a = (x - i as f64) as f32;
    let j = (i + 1) % n;
    f[i] += (1.0 - a) * v;
    wf[i] += (1.0 - a) * w;
    f[j] += a * v;
    wf[j] += a * w;
}

/// The frame holds together: its missing pulse and markers where they
/// belong (one may be lost in a fade; the bits around it answer for
/// themselves), and no unused bit clearly 1.
fn structure_ok(syms: &[Sym]) -> bool {
    let mut misses = 0;
    for (pos, s) in syms.iter().enumerate() {
        match slot(pos) {
            Slot::Hole if s.class() != HOLE => misses += 1,
            Slot::Marker if s.class() != MARK => misses += 1,
            Slot::Unused if s.ll[ONE] - s.ll[ZERO] > UNUSED_REJECT => return false,
            _ => {}
        }
    }
    misses <= 1
}

/// The time fields of a frame's bits, if every BCD digit and the date are
/// in range.
fn decode_time(bit: impl Fn(usize) -> bool) -> Option<UtcMinute> {
    let num = |first: usize, n: usize| (0..n).map(|k| u32::from(bit(first + k)) << k).sum::<u32>();
    let digits = [num(4, 4), num(51, 4), num(10, 4), num(15, 3), num(20, 4), num(25, 2), num(30, 4), num(35, 4)];
    if digits.iter().any(|&d| d > 9) {
        return None;
    }
    let year = 2000 + digits[1] * 10 + digits[0];
    let minute = digits[3] * 10 + digits[2];
    let hour = digits[5] * 10 + digits[4];
    let day = num(40, 2) * 100 + digits[7] * 10 + digits[6];
    let days = if is_leap(i64::from(year)) { 366 } else { 365 };
    if minute > 59 || hour > 23 || day == 0 || day > days {
        return None;
    }
    Some(UtcMinute { year: year as u16, day_of_year: day as u16, hour: hour as u8, minute: minute as u8 })
}

/// The time fields' bits of a minute.
fn encode_time(t: UtcMinute) -> [bool; 60] {
    let mut b = [false; 60];
    let mut put = |first: usize, n: usize, v: u32| {
        for k in 0..n {
            b[first + k] = (v >> k) & 1 == 1;
        }
    };
    let yy = u32::from(t.year % 100);
    let (mi, h, d) = (u32::from(t.minute), u32::from(t.hour), u32::from(t.day_of_year));
    put(4, 4, yy % 10);
    put(51, 4, yy / 10);
    put(10, 4, mi % 10);
    put(15, 3, mi / 10);
    put(20, 4, h % 10);
    put(25, 2, h / 10);
    put(30, 4, d % 10);
    put(35, 4, d / 10 % 10);
    put(40, 2, d / 100);
    b
}

/// A frame's soft bits: the log-likelihood ratio of 1 over 0 at each
/// second.
#[derive(Clone, Copy)]
struct Soft {
    j_end: u64,
    llr: [f32; 60],
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Reading {
    utc: UtcMinute,
    dst1: bool,
    dst2: bool,
    leap: bool,
    dut1: f32,
    confidence: f32,
    frame_confidence: f32,
    minutes: u8,
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

/// The newest frame of `w` (oldest first, all on one minute alignment),
/// read with the frames before it.
///
/// The time is the maximum-likelihood one over all the frames' soft bits,
/// each frame scored against the candidate counted back to it. Candidates
/// are each frame's own reading, and that reading with one of its three
/// weakest bits changed, counted forward. The probability that the time is
/// wrong is bounded by the summed likelihood, relative to the best, of
/// every other candidate and of every time one bit away from it. DUT1,
/// DST and the leap warning change only at 00:00 UTC, so their bits are
/// summed over the frames of the same UTC day. The newest frame must not
/// itself contradict the result by more than [`CONTRADICT`] on any bit: a
/// minute that was not heard is not reported.
fn combine(w: &[Soft]) -> Option<Reading> {
    let newest = w.last()?;
    let back = |k: &Soft| ((newest.j_end - k.j_end) / 60) as i64;
    let mut cands: Vec<UtcMinute> = Vec::new();
    for k in w {
        let mut order = TIME_BITS;
        order.sort_by(|&a, &b| k.llr[a].abs().total_cmp(&k.llr[b].abs()));
        for flip in [None, Some(order[0]), Some(order[1]), Some(order[2])] {
            let t = decode_time(|b| (k.llr[b] > 0.0) != (Some(b) == flip));
            if let Some(t) = t.map(|t| t.add_minutes(back(k)))
                && !cands.contains(&t)
            {
                cands.push(t);
            }
        }
    }
    let ll = |t: UtcMinute| -> f32 {
        w.iter()
            .map(|k| {
                let e = encode_time(t.add_minutes(-back(k)));
                TIME_BITS.iter().map(|&b| if e[b] { 0.5 * k.llr[b] } else { -0.5 * k.llr[b] }).sum::<f32>()
            })
            .sum()
    };
    let scored: Vec<(UtcMinute, f32)> = cands.iter().map(|&t| (t, ll(t))).collect();
    let &(best, lb) = scored.iter().max_by(|a, b| a.1.total_cmp(&b.1))?;
    let e = encode_time(best);
    let mut p_err: f32 = scored.iter().filter(|c| c.0 != best).map(|c| (c.1 - lb).min(0.0).exp()).sum();
    for &b in &TIME_BITS {
        let mut f = e;
        f[b] = !f[b];
        if let Some(t) = decode_time(|p| f[p])
            && !cands.contains(&t)
        {
            p_err += (ll(t) - lb).min(0.0).exp();
        }
    }
    let mut confidence = (1.0 - p_err).max(0.0);

    let same_day = |k: &&Soft| {
        let t = best.add_minutes(-back(k));
        (t.year, t.day_of_year) == (best.year, best.day_of_year)
    };
    let mut want = e;
    let mut steady = [0f32; 60];
    for &b in &STEADY_BITS {
        steady[b] = w.iter().filter(same_day).map(|k| k.llr[b]).sum();
        want[b] = steady[b] > 0.0;
    }
    let mag = (u32::from(want[56]) + 2 * u32::from(want[57]) + 4 * u32::from(want[58])) as f32 / 10.0;
    for &b in &STEADY_BITS {
        // With no correction its sign does not matter.
        if b != 50 || mag > 0.0 {
            confidence *= sigmoid(steady[b].abs());
        }
    }
    let mut frame_confidence = 1.0;
    for &b in TIME_BITS.iter().chain(&STEADY_BITS) {
        let l = if want[b] { newest.llr[b] } else { -newest.llr[b] };
        if l < -CONTRADICT {
            return None;
        }
        frame_confidence *= sigmoid(l);
    }
    Some(Reading {
        utc: best,
        dst1: want[2],
        dst2: want[55],
        leap: want[3],
        dut1: if want[50] { mag } else { -mag },
        confidence,
        frame_confidence,
        minutes: w.len() as u8,
    })
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtty::testsig::{Rng, rayleigh};

    /// What the live server delivers.
    const FS: f32 = 47_904.0;

    #[derive(Clone, Copy, Debug)]
    struct Info {
        dut1_tenths: i8,
        dst1: bool,
        dst2: bool,
        leap: bool,
    }

    const PLAIN: Info = Info { dut1_tenths: -3, dst1: false, dst2: false, leap: false };

    /// The 60 symbols of a minute: 0, 1, 2 for a marker, 3 for second 0.
    fn symbols(utc: UtcMinute, info: Info) -> [u8; 60] {
        let mut s = [0u8; 60];
        s[0] = 3;
        for p in [9, 19, 29, 39, 49, 59] {
            s[p] = 2;
        }
        let mut put = |first: usize, n: usize, v: u32| {
            for k in 0..n {
                s[first + k] = ((v >> k) & 1) as u8;
            }
        };
        let yy = u32::from(utc.year % 100);
        let (mi, h, d) = (u32::from(utc.minute), u32::from(utc.hour), u32::from(utc.day_of_year));
        put(4, 4, yy % 10);
        put(51, 4, yy / 10);
        put(10, 4, mi % 10);
        put(15, 3, mi / 10);
        put(20, 4, h % 10);
        put(25, 2, h / 10);
        put(30, 4, d % 10);
        put(35, 4, d / 10 % 10);
        put(40, 2, d / 100);
        put(56, 3, u32::from(info.dut1_tenths.unsigned_abs()));
        s[50] = u8::from(info.dut1_tenths >= 0);
        s[2] = u8::from(info.dst1);
        s[55] = u8::from(info.dst2);
        s[3] = u8::from(info.leap);
        s
    }

    /// One station's audio (its modulation, 1.0 = 100 %) at transmitted
    /// UTC `u` (unix seconds).
    struct Voice {
        station: Station,
        info: Info,
        code: bool,
        minute: i64,
        syms: [u8; 60],
    }

    impl Voice {
        fn new(station: Station, info: Info) -> Self {
            Self { station, info, code: true, minute: i64::MIN, syms: [0; 60] }
        }

        fn at(&mut self, u: f64) -> f64 {
            let sec = u.floor();
            let frac = u - sec;
            let sec = sec as i64;
            let s = sec.rem_euclid(60);
            let start = sec - s;
            if start != self.minute {
                self.minute = start;
                self.syms = symbols(UtcMinute::from_unix_s(start), self.info);
            }
            let moh = (start / 60).rem_euclid(60);
            let tick = f64::from(self.station.tick_hz());
            let mut m = 0.0;
            // The tick, or the minute or hour tone, at 100 %.
            if s == 0 {
                if frac < 0.8 {
                    let f = if moh == 0 { 1500.0 } else { tick };
                    m += (TAU * f * frac).sin();
                }
            } else if s != 29 && s != 59 && frac < TICK_S {
                m += (TAU * tick * frac).sin();
            }
            // Outside the protected zone: the standard tones at 50 % and the
            // 100 Hz code, high until its pulse ends and low after.
            if (0.030..0.990).contains(&frac) {
                if (1..=44).contains(&s) {
                    let hour_tone = if self.station == Station::Wwv { 2 } else { 1 };
                    let f = if moh == hour_tone {
                        440.0
                    } else if moh % 2 == 0 {
                        500.0
                    } else {
                        600.0
                    };
                    m += 0.5 * (TAU * f * (s as f64 + frac)).sin();
                }
                if self.code {
                    let end = match self.syms[s as usize] {
                        0 => 0.2,
                        1 => 0.5,
                        2 => 0.8,
                        _ => 0.0,
                    };
                    let a = if frac < end { 0.18 } else { 0.03 };
                    m += a * (TAU * SUBCARRIER_HZ * frac).sin();
                }
            }
            m
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum Fading {
        None,
        /// One Rayleigh path, Doppler spread in Hz.
        Flat(f32),
        /// Two equal Rayleigh paths: delay in ms, spread in Hz.
        Watterson(f32, f32),
    }

    struct Tx {
        station: Station,
        /// Propagation delay, s.
        delay: f64,
        /// Carrier amplitude.
        gain: f64,
        fading: Fading,
    }

    struct Scene {
        /// True UTC of the first sample.
        start: f64,
        secs: f64,
        /// Sample clock error, ppm (positive: samples come faster).
        ppm: f64,
        offset_hz: f64,
        /// Carrier to noise in 2500 Hz, against the first station's mean
        /// carrier; None for no noise.
        cnr_db: Option<f32>,
        txs: Vec<Tx>,
        info: Info,
        code: bool,
        /// Wall clock less true time.
        clock_err_ms: f64,
        seed: u64,
        /// Input level.
        level: f64,
    }

    impl Scene {
        fn new(start: f64, secs: f64, txs: Vec<Tx>) -> Self {
            Self {
                start,
                secs,
                ppm: 0.0,
                offset_hz: 0.0,
                cnr_db: None,
                txs,
                info: PLAIN,
                code: true,
                clock_err_ms: 0.0,
                seed: 1,
                level: 1.0,
            }
        }
    }

    fn tx(station: Station, delay_ms: f64, gain: f64, fading: Fading) -> Tx {
        Tx { station, delay: delay_ms / 1000.0, gain, fading }
    }

    /// Unix seconds of a UTC date and time.
    fn unix(y: i64, mo: u32, d: u32, h: i64, mi: i64, s: f64) -> f64 {
        (days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60) as f64 + s
    }

    /// Generate the scene in irregular chunks into `f`, with each chunk's
    /// wall-clock time.
    fn generate(sc: &Scene, mut f: impl FnMut(&[Complex32], f64)) {
        const SIZES: [usize; 6] = [480, 960, 333, 2000, 717, 1440];
        let fs_true = f64::from(FS) * (1.0 + sc.ppm * 1e-6);
        let total = (sc.secs * fs_true) as usize;
        let mut rng = Rng::new(sc.seed);
        let mut voices: Vec<Voice> = sc
            .txs
            .iter()
            .map(|t| {
                let mut v = Voice::new(t.station, sc.info);
                v.code = sc.code;
                v
            })
            .collect();
        // Fading gains at 64 Hz, interpolated.
        let slow_n = (sc.secs * 64.0) as usize + 4;
        let gains: Vec<[Vec<Complex32>; 2]> = sc
            .txs
            .iter()
            .map(|t| match t.fading {
                Fading::None => [Vec::new(), Vec::new()],
                Fading::Flat(s) => [rayleigh(slow_n, 64.0, s, &mut rng), Vec::new()],
                Fading::Watterson(_, s) => [rayleigh(slow_n, 64.0, s, &mut rng), rayleigh(slow_n, 64.0, s, &mut rng)],
            })
            .collect();
        let g_at = |g: &[Complex32], t: f64| {
            let x = t * 64.0;
            let k = x as usize;
            let a = (x - k as f64) as f32;
            let v = g[k] * (1.0 - a) + g[k + 1] * a;
            Complex64::new(f64::from(v.re), f64::from(v.im))
        };
        let sigma = sc.cnr_db.map(|c| {
            let g = if sc.txs[0].gain > 0.0 { sc.txs[0].gain } else { 1.0 };
            let n0 = g * g / (2500.0 * 10f64.powf(f64::from(c) / 10.0));
            (n0 * fs_true / 2.0).sqrt() * sc.level
        });
        let mut chunk = Vec::new();
        let (mut i, mut k) = (0usize, 0usize);
        while i < total {
            let n = SIZES[k % SIZES.len()].min(total - i);
            chunk.clear();
            for ii in i..i + n {
                let t = ii as f64 / fs_true;
                let utc = sc.start + t;
                let mut z = Complex64::default();
                for (ti, (txp, v)) in sc.txs.iter().zip(voices.iter_mut()).enumerate() {
                    let a = txp.gain * (1.0 + v.at(utc - txp.delay));
                    z += match txp.fading {
                        Fading::None => Complex64::new(a, 0.0),
                        Fading::Flat(_) => g_at(&gains[ti][0], t) * a,
                        Fading::Watterson(d, _) => {
                            let a2 = txp.gain * (1.0 + v.at(utc - txp.delay - f64::from(d) / 1000.0));
                            (g_at(&gains[ti][0], t) * a + g_at(&gains[ti][1], t) * a2) * std::f64::consts::FRAC_1_SQRT_2
                        }
                    };
                }
                z *= Complex64::from_polar(sc.level, TAU * (sc.offset_hz * t).fract() + 0.7);
                if let Some(s) = sigma {
                    let (u1, u2) = (rng.uniform(), rng.uniform());
                    let r = (-2.0 * u1.ln()).sqrt() * s;
                    z += Complex64::from_polar(r, TAU * u2);
                }
                chunk.push(Complex32::new(z.re as f32, z.im as f32));
            }
            let wall = (sc.start + i as f64 / fs_true) * 1000.0 + sc.clock_err_ms;
            f(&chunk, wall);
            i += n;
            k += 1;
        }
    }

    struct Run {
        frames: Vec<MinuteFrame>,
        status: TimecodeStatus,
    }

    fn run(sc: &Scene) -> Run {
        let mut d = TimecodeDecoder::new(FS);
        let mut frames = Vec::new();
        generate(sc, |c, wall| {
            d.push_iq_at(c, wall);
            for e in d.take() {
                let TimecodeEvent::Minute(f) = e;
                frames.push(f);
            }
        });
        Run { frames, status: d.status() }
    }

    /// The minutes that start within the scene with all of their seconds
    /// heard.
    fn expected(sc: &Scene) -> Vec<UtcMinute> {
        let delay = sc.txs[0].delay;
        let first = ((sc.start - delay) / 60.0).ceil() as i64;
        let last = ((sc.start + sc.secs - delay - 60.5) / 60.0).floor() as i64;
        (first..=last).map(|m| UtcMinute::from_unix_s(m * 60)).collect()
    }

    fn check_fields(f: &MinuteFrame, info: Info) {
        assert!((f.dut1_s - f32::from(info.dut1_tenths) / 10.0).abs() < 1e-6, "{f:?}");
        assert_eq!((f.dst1, f.dst2, f.leap_second_warning), (info.dst1, info.dst2, info.leap), "{f:?}");
    }

    #[test]
    fn timecode_calendar_round_trips() {
        let t = UtcMinute { year: 2024, day_of_year: 366, hour: 23, minute: 59 };
        assert_eq!(t.month_day(), (12, 31));
        assert_eq!(t.add_minutes(1), UtcMinute { year: 2025, day_of_year: 1, hour: 0, minute: 0 });
        assert_eq!(UtcMinute::from_unix_s(t.unix_s()), t);
        assert_eq!(UtcMinute::from_unix_s(1_767_225_600).iso(), "2026-01-01T00:00Z");
        assert_eq!(UtcMinute { year: 2026, day_of_year: 60, hour: 0, minute: 0 }.month_day(), (3, 1));
    }

    #[test]
    fn timecode_dst_bits() {
        assert_eq!(Dst::from_bits(false, false), Dst::Standard);
        assert_eq!(Dst::from_bits(true, true), Dst::Daylight);
        // DST2 goes first, at 00:00 UTC on the day of the change.
        assert_eq!(Dst::from_bits(false, true), Dst::BeginsToday);
        assert_eq!(Dst::from_bits(true, false), Dst::EndsToday);
    }

    /// Symbols as certain soft decisions.
    fn clean(s: &[u8; 60]) -> Vec<Sym> {
        s.iter()
            .enumerate()
            .map(|(p, &v)| {
                let c = match v {
                    0 => ZERO,
                    1 => ONE,
                    2 => MARK,
                    _ => HOLE,
                };
                let mut ll = [-50.0; 4];
                ll[c] = 0.0;
                Sym { j: p as u64, ll, start: 0.0 }
            })
            .collect()
    }

    /// A frame's soft bits at `|llr|` each, ending at second `j_end`.
    fn soft(s: &[u8; 60], llr: f32, j_end: u64) -> Soft {
        let mut l = [0f32; 60];
        for (x, &v) in l.iter_mut().zip(s) {
            *x = if v == 1 { llr } else { -llr };
        }
        Soft { j_end, llr: l }
    }

    #[test]
    fn timecode_frame_fields_and_checks() {
        let utc = UtcMinute { year: 2026, day_of_year: 310, hour: 14, minute: 37 };
        let info = Info { dut1_tenths: 2, dst1: true, dst2: false, leap: true };
        let s = symbols(utc, info);
        assert!(structure_ok(&clean(&s)));
        let r = combine(&[soft(&s, 20.0, 59)]).unwrap();
        assert_eq!(r.utc, utc);
        assert_eq!((r.dst1, r.dst2, r.leap), (true, false, true));
        assert!((r.dut1 - 0.2).abs() < 1e-6);
        assert!(r.confidence > 0.999 && r.frame_confidence > 0.999 && r.minutes == 1);
        // A negative DUT1.
        let s2 = symbols(utc, Info { dut1_tenths: -7, ..info });
        assert!((combine(&[soft(&s2, 20.0, 59)]).unwrap().dut1 + 0.7).abs() < 1e-6);
        // An unused bit set, or two missing markers, and the frame does not
        // hold together; one missing marker is survived.
        let mut bad = s;
        bad[44] = 1;
        assert!(!structure_ok(&clean(&bad)));
        let mut bad = s;
        bad[19] = 1;
        bad[39] = 0;
        assert!(!structure_ok(&clean(&bad)));
        let mut one = s;
        one[29] = 1;
        assert!(structure_ok(&clean(&one)));
        // A minute digit out of range, a day past the year's end: no time.
        let mut bad = s;
        bad[10..14].copy_from_slice(&[0, 1, 0, 1]);
        assert!(combine(&[soft(&bad, 20.0, 59)]).is_none());
        let bad = symbols(UtcMinute { day_of_year: 366, ..utc }, info);
        assert!(combine(&[soft(&bad, 20.0, 59)]).is_none());
    }

    #[test]
    fn timecode_minutes_read_together() {
        // Five minutes across a new year, each with one bit read wrong (a
        // different one each minute) and doubtful: alone, every one is
        // wrong or out of range; together they give the time.
        let t0 = UtcMinute { year: 2025, day_of_year: 365, hour: 23, minute: 57 };
        let flips = [11usize, 21, 31, 52, 5];
        let mut w = Vec::new();
        for (k, &f) in flips.iter().enumerate() {
            let s = symbols(t0.add_minutes(k as i64), PLAIN);
            let mut sf = soft(&s, 4.0, 59 + 60 * k as u64);
            sf.llr[f] *= -0.5;
            assert_ne!(decode_time(|b| sf.llr[b] > 0.0), Some(t0.add_minutes(k as i64)));
            w.push(sf);
        }
        let r = combine(&w).unwrap();
        assert_eq!(r.utc, UtcMinute { year: 2026, day_of_year: 1, hour: 0, minute: 1 });
        assert!((r.dut1 + 0.3).abs() < 1e-6);
        assert!(r.confidence > 0.99, "{r:?}");
        assert!(r.frame_confidence < 0.5, "{r:?}");
        assert_eq!(r.minutes, 5);
        // A newest frame that firmly says otherwise is not overruled.
        let mut last = *w.last().unwrap();
        last.llr[12] = -last.llr[12].signum() * 10.0;
        let n = w.len();
        w[n - 1] = last;
        assert!(combine(&w).is_none());
    }

    /// WWV across a new year, with a negative DUT1, a fast sample clock,
    /// a carrier offset and a known path delay and clock error.
    #[test]
    fn timecode_decodes_minutes_across_new_year() {
        let mut sc = Scene::new(unix(2025, 12, 31, 23, 56, 37.0), 330.0, vec![tx(Station::Wwv, 9.7, 1.0, Fading::None)]);
        sc.ppm = 100.0;
        sc.offset_hz = 23.0;
        sc.cnr_db = Some(10.0);
        sc.clock_err_ms = 3.2;
        sc.level = 0.003;
        let r = run(&sc);
        let want = expected(&sc);
        let got: Vec<UtcMinute> = r.frames.iter().map(|f| f.utc).collect();
        assert_eq!(got, want, "{:#?}", r.frames);
        assert_eq!(want[3], UtcMinute { year: 2026, day_of_year: 1, hour: 0, minute: 0 });
        for (i, f) in r.frames.iter().enumerate() {
            check_fields(f, PLAIN);
            assert_eq!(f.dst, Dst::Standard);
            assert_eq!(f.minutes_used as usize, i + 1, "{f:?}");
            assert!(f.confidence > 0.99, "{f:?}");
            assert_eq!(f.station, Some(Station::Wwv));
            let t = f.tick_offset_ms.unwrap();
            assert!((t - 12.9).abs() < 0.3, "tick offset {t}");
            let c = f.clock_offset_ms.unwrap();
            assert!((c - 12.9).abs() < 0.3, "clock offset {c}");
        }
        let st = &r.status;
        println!("{st:?}");
        assert!(st.locked);
        assert_eq!(st.station, Some(Station::Wwv));
        assert!(st.wwv_db.is_some() && st.wwvh_db.is_none());
        assert!((st.carrier_offset_hz.unwrap() - 23.0).abs() < 1.0);
        let snr = st.snr_db.unwrap();
        assert!((snr - (10.0 - 3.9)).abs() < 1.5, "snr {snr}");
        assert_eq!(st.last_utc, want.last().copied());
        // The bits shown are the current minute's so far.
        let sec = st.second.unwrap() as usize;
        assert!(st.bits[..sec].iter().all(Option::is_some) && st.bits[sec..].iter().all(Option::is_none) || sec == 0);
        assert_eq!(st.bits[0], Some(BIT_HOLE));
    }

    #[test]
    fn timecode_identifies_wwvh() {
        let mut sc = Scene::new(unix(2026, 6, 21, 11, 2, 12.0), 200.0, vec![tx(Station::Wwvh, 27.4, 1.0, Fading::None)]);
        sc.ppm = -60.0;
        sc.offset_hz = -41.0;
        sc.cnr_db = Some(12.0);
        sc.info = Info { dut1_tenths: 2, dst1: true, dst2: true, leap: true };
        sc.level = 40.0;
        let r = run(&sc);
        let got: Vec<UtcMinute> = r.frames.iter().map(|f| f.utc).collect();
        assert_eq!(got, expected(&sc), "{:#?}", r.frames);
        for f in &r.frames {
            check_fields(f, sc.info);
            assert_eq!(f.dst, Dst::Daylight);
            assert_eq!(f.station, Some(Station::Wwvh));
            assert!((f.tick_offset_ms.unwrap() - 27.4).abs() < 0.3, "{f:?}");
        }
        let st = &r.status;
        assert_eq!(st.station, Some(Station::Wwvh));
        assert!(st.wwvh_db.is_some() && st.wwv_db.is_none(), "{st:?}");
    }

    #[test]
    fn timecode_hears_both_stations() {
        // WWV strong, WWVH 8 dB down and 21 ms later, each fading on its own.
        let mut sc = Scene::new(
            unix(2026, 3, 8, 4, 10, 40.0),
            200.0,
            vec![tx(Station::Wwv, 6.1, 1.0, Fading::Flat(0.2)), tx(Station::Wwvh, 27.3, 0.4, Fading::Flat(0.2))],
        );
        sc.cnr_db = Some(18.0);
        sc.ppm = 30.0;
        sc.offset_hz = 5.0;
        sc.info = Info { dut1_tenths: 0, dst1: false, dst2: true, leap: false };
        sc.seed = 4;
        let r = run(&sc);
        let st = &r.status;
        println!("{st:?}");
        assert!(st.wwv_db.is_some() && st.wwvh_db.is_some(), "{st:?}");
        assert_eq!(st.station, Some(Station::Wwv));
        assert!((st.wwv_tick_ms.unwrap() - 6.1).abs() < 0.5, "{st:?}");
        assert!((st.wwvh_tick_ms.unwrap() - 27.3).abs() < 0.5, "{st:?}");
        let want = expected(&sc);
        for f in &r.frames {
            assert!(want.contains(&f.utc), "{f:?}");
            check_fields(f, sc.info);
            assert_eq!(f.dst, Dst::BeginsToday);
        }
        assert!(r.frames.len() >= want.len() - 1, "{} of {}", r.frames.len(), want.len());
    }

    /// Minutes read of those sent through `fading` at `cnr`, and how many
    /// of those reported were wrong.
    fn trial(fading: Fading, cnr: f32, secs: f64, seed: u64) -> (usize, usize, usize) {
        let mut sc = Scene::new(unix(2026, 11, 6, 17, 3, 30.0 + seed as f64), secs, vec![tx(Station::Wwv, 11.0, 1.0, fading)]);
        sc.cnr_db = Some(cnr);
        sc.ppm = if seed.is_multiple_of(2) { 70.0 } else { -70.0 };
        sc.offset_hz = 12.0;
        sc.seed = seed;
        let r = run(&sc);
        let want = expected(&sc);
        let wrong = r.frames.iter().filter(|f| !want.contains(&f.utc) || f.dut1_s != -0.3).count();
        if std::env::var("USDR_TIMECODE_DEBUG").is_ok() {
            for f in &r.frames {
                let ok = want.contains(&f.utc) && f.dut1_s == -0.3;
                println!("{} {} conf {:.4} own {:.4}", if ok { "ok   " } else { "WRONG" }, f.utc.iso(), f.confidence, f.frame_confidence);
            }
        }
        (r.frames.len() - wrong, want.len(), wrong)
    }

    #[test]
    fn timecode_reads_through_noise() {
        // At 3 dB carrier to noise in 2500 Hz every minute from the third
        // on is read (alone, about one in two would be), and none wrongly.
        let (mut ok, mut all) = (0, 0);
        for seed in 1..=2 {
            let (o, a, w) = trial(Fading::None, 3.0, 370.0, 40 + seed);
            assert_eq!(w, 0);
            assert!(o + 2 >= a, "{o} of {a}");
            ok += o;
            all += a;
        }
        println!("AWGN 3 dB: {ok} of {all} minutes");
    }

    #[test]
    fn timecode_reads_through_fading() {
        for (fading, cnr, min_share) in [(Fading::Flat(0.5), 15.0, 0.6), (Fading::Watterson(1.0, 0.5), 15.0, 0.6)] {
            let (o, a, w) = trial(fading, cnr, 370.0, 7);
            println!("{fading:?} at {cnr} dB: {o} of {a} minutes, {w} wrong");
            assert_eq!(w, 0, "{fading:?}");
            assert!(o as f32 >= min_share * a as f32, "{fading:?}: {o} of {a}");
        }
    }

    /// Symbol decisions against what was sent, for tuning.
    #[test]
    fn timecode_diagnose() {
        let Ok(spec) = std::env::var("USDR_TIMECODE_DIAG") else { return };
        let v: Vec<f64> = spec.split(',').map(|x| x.parse().unwrap()).collect();
        let fading = match v[0] as u32 {
            0 => Fading::None,
            1 => Fading::Flat(0.5),
            2 => Fading::Watterson(1.0, 0.5),
            _ => Fading::Watterson(2.0, 1.0),
        };
        let seed = v[2] as u64;
        let mut sc = Scene::new(unix(2026, 11, 6, 17, 3, 30.0 + seed as f64), 370.0, vec![tx(Station::Wwv, 11.0, 1.0, fading)]);
        sc.cnr_db = Some(v[1] as f32);
        sc.seed = seed;
        sc.ppm = v.get(3).copied().unwrap_or(if seed.is_multiple_of(2) { 70.0 } else { -70.0 });
        sc.offset_hz = 12.0;
        let fs_true = f64::from(FS) * (1.0 + sc.ppm * 1e-6);
        let mut d = TimecodeDecoder::new(FS);
        let mut conf = [[0u32; 4]; 4];
        let mut seen = std::collections::HashSet::new();
        let mut frames = Vec::new();
        let mut log = Vec::new();
        let mut last_sec = 0;
        generate(&sc, |c, w| {
            d.push_iq_at(c, w);
            frames.extend(d.take());
            if std::env::var("USDR_TIMECODE_TICKS").is_ok() && d.sec != last_sec {
                last_sec = d.sec;
                let t = &d.ticks[0];
                println!("sec {} tick {:?} drift {:.1} src {} phase {:.6} tphase {:.6}", d.sec, t.offset_ms, d.drift * 1e6, d.drift_src, t.phase, d.tick_phase);
            }
            for sy in &d.syms {
                if !seen.insert(sy.j) {
                    continue;
                }
                let t = d.input_index(sy.start) / fs_true;
                let u = sc.start + t - sc.txs[0].delay;
                let sec = u.round() as i64;
                let truth = symbols(UtcMinute::from_unix_s(sec - sec.rem_euclid(60)), sc.info)[sec.rem_euclid(60) as usize];
                let tc = [ZERO, ONE, MARK, HOLE][truth as usize];
                conf[tc][sy.class()] += 1;
                log.push(format!(
                    "{:>3} t{} d{} err {:+.1} ms ll {:?}",
                    sec.rem_euclid(60),
                    tc,
                    sy.class(),
                    (u - u.round()) * 1000.0,
                    sy.ll.map(|x| x.round() as i32)
                ));
            }
        });
        if std::env::var("USDR_TIMECODE_LOG").is_ok() {
            for l in &log {
                println!("{l}");
            }
        }
        println!("rows truth (hole 0 1 marker), columns decided: {conf:?}");
        println!("drift {:.2} ppm", d.drift * 1e6);
        println!("high {} low {} frames {} nu_q {:.3e} nu_res {:.3e}", d.high, d.low, frames.len(), d.nu_num / d.nu_den, d.res_num / d.res_den);
        for f in &frames {
            let TimecodeEvent::Minute(m) = f;
            println!("{} {:.4} tick {:?} clock {:?}", f.summary(), m.confidence, m.tick_offset_ms, m.clock_offset_ms);
        }
    }

    #[test]
    fn timecode_table() {
        if std::env::var("USDR_TIMECODE_TABLE").is_err() {
            return;
        }
        let cnrs = [12.0f32, 8.0, 5.0, 3.0, 1.0, -1.0];
        println!("Minutes read (wrong) of those sent, by carrier to noise in 2500 Hz");
        for fading in [Fading::None, Fading::Flat(0.5), Fading::Watterson(1.0, 0.5), Fading::Watterson(2.0, 1.0)] {
            let mut row = String::new();
            for &c in &cnrs {
                let (mut o, mut a, mut w) = (0, 0, 0);
                for seed in 1..=2 {
                    let (oo, aa, ww) = trial(fading, c, 370.0, 100 + seed);
                    o += oo;
                    a += aa;
                    w += ww;
                }
                row += &format!(" {c:>4}: {o:>2}/{a} ({w})");
            }
            println!("{:>24}{row}", format!("{fading:?}"));
        }
    }

    #[test]
    fn timecode_noise_and_bare_carrier_give_nothing() {
        // Noise alone at three levels, and a carrier with ticks but no code.
        for (seed, level) in [(1u64, 1e-4), (2, 1.0), (3, 300.0)] {
            let mut sc = Scene::new(unix(2026, 1, 1, 0, 0, 0.0), 150.0, vec![tx(Station::Wwv, 0.0, 0.0, Fading::None)]);
            sc.cnr_db = Some(0.0);
            sc.level = level;
            sc.seed = seed;
            let mut d = TimecodeDecoder::new(FS);
            let mut n = 0;
            generate(&sc, |c, w| {
                d.push_iq_at(c, w);
                n += d.take().len();
            });
            let st = d.status();
            assert_eq!(n, 0);
            assert!(!st.locked && st.station.is_none(), "{st:?}");
        }
        let mut sc = Scene::new(unix(2026, 1, 1, 0, 0, 0.0), 150.0, vec![tx(Station::Wwv, 3.0, 1.0, Fading::None)]);
        sc.code = false;
        sc.cnr_db = Some(10.0);
        let r = run(&sc);
        assert!(r.frames.is_empty());
        assert!(!r.status.locked, "{:?}", r.status);
        // The ticks are still heard.
        assert_eq!(r.status.station, Some(Station::Wwv));
    }

    #[test]
    fn timecode_without_timestamps_reports_no_offsets() {
        let mut sc = Scene::new(unix(2026, 2, 1, 8, 0, 50.0), 140.0, vec![tx(Station::Wwv, 5.0, 1.0, Fading::None)]);
        sc.cnr_db = Some(15.0);
        let mut d = TimecodeDecoder::new(FS);
        let mut frames = Vec::new();
        generate(&sc, |c, _| {
            d.push_iq(c);
            frames.extend(d.take());
        });
        assert_eq!(frames.len(), expected(&sc).len());
        let TimecodeEvent::Minute(f) = &frames[0];
        assert!(f.tick_offset_ms.is_none() && f.clock_offset_ms.is_none());
        let st = d.status();
        assert!(st.tick_offset_ms.is_none() && st.station == Some(Station::Wwv));
        assert_eq!(frames[0].kind(), "minute");
        println!("{}", frames[0].summary());
        let fields = frames[0].fields();
        assert!(fields.iter().any(|(k, v)| *k == "utc" && v == "2026-02-01T08:01Z"), "{fields:?}");
    }

    #[test]
    fn timecode_realtime_factor() {
        let mut sc = Scene::new(unix(2026, 11, 6, 12, 0, 0.0), 60.0, vec![tx(Station::Wwv, 9.0, 1.0, Fading::None)]);
        sc.cnr_db = Some(5.0);
        let mut chunks = Vec::new();
        generate(&sc, |c, w| chunks.push((c.to_vec(), w)));
        let mut d = TimecodeDecoder::new(FS);
        let t = std::time::Instant::now();
        for (c, w) in &chunks {
            d.push_iq_at(c, *w);
        }
        let el = t.elapsed().as_secs_f64();
        println!("timecode: 60 s of I/Q at {FS} Hz in {:.1} ms, {:.3}% of one core", el * 1e3, 100.0 * el / 60.0);
        assert!(el / 60.0 < 0.05);
    }
}
