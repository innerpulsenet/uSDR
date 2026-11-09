//! What the whole span held, one row a minute, for the all-day chart on the site.
//!
//! An average of a band is mostly noise, because noise is always there and
//! signals come and go. So each row keeps a high percentile over the minute
//! instead: anything present for a second or so shows at its real level, and a
//! lightning click too short to matter does not.
//!
//! Frequencies are cells on a fixed grid from 0 Hz, so a row means the same thing
//! wherever the tuner sat, and a minute that spans a retune is the union of both
//! positions. A cell the tuner did not cover long enough is left out, never
//! filled in, and the chart hatches it.
//!
//! Each frame is a short FFT taken ten times a second from one block in hundreds,
//! so this costs almost nothing beside the decoders. It only reads the capture.

use std::collections::VecDeque;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};

use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use serde_json::{Value, json};

/// The width of one cell.
pub const GRID_HZ: f64 = 10_000.0;
/// Cells from 0 Hz to 30 MHz.
pub const GRID_BINS: usize = 3_000;
const FFT_LEN: usize = 4_096;
const FRAME_EVERY_MS: u64 = 100;
/// A cell needs this many frames in the minute, six seconds' worth, to be reported.
const MIN_FRAMES: usize = 60;
/// Of about 600 frames, the level exceeded by the top 12: a signal on for over a
/// second in the minute shows.
const PERCENTILE: usize = 98;
/// How much of the span is good enough to show, either side of center.
const USABLE: f64 = 0.46;
/// The tuner's own leftovers sit here, so these cells are left out.
const DC_GUARD_HZ: f64 = 6_000.0;
/// Frames just after a retune still hold the old frequency or a settling gain.
const SETTLE_MS: u64 = 500;
/// Six hours waiting for the uplink. Older rows are dropped, and the chart hatches them.
const KEEP_ROWS: usize = 360;
const QUEUE: usize = 16;
/// A cell with no reading. Readings start at 1.
const NO_DATA: u8 = 0;

static PENDING: Mutex<VecDeque<Row>> = Mutex::new(VecDeque::new());

/// One minute. `data[i]` is cell `lo + i`, in half-dB steps above -140 dBFS.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub minute: u64,
    pub lo: usize,
    pub data: Vec<u8>,
}

struct Frame {
    iq: Vec<Complex32>,
    rate: f64,
    center_hz: f64,
    ms: u64,
}

fn quantize(power: f32) -> u8 {
    let db = 10.0 * power.max(1e-20).log10();
    ((db + 140.0) * 2.0).round().clamp(1.0, 255.0) as u8
}

struct Accumulator {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    /// Scales a full-scale tone to 0 dBFS.
    gain: f32,
    buffer: Vec<Complex32>,
    scratch: Vec<Complex32>,
    cells: Vec<Vec<u8>>,
    peak: Vec<f32>,
    minute: Option<u64>,
    key: Option<(f64, f64)>,
    settled_ms: u64,
}

impl Accumulator {
    fn new() -> Self {
        let fft = FftPlanner::new().plan_fft_forward(FFT_LEN);
        let window: Vec<f32> = (0..FFT_LEN).map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / FFT_LEN as f32).cos()).collect();
        let sum: f32 = window.iter().sum();
        let scratch = vec![Complex32::default(); fft.get_inplace_scratch_len()];
        Self {
            fft,
            window,
            gain: sum * sum,
            buffer: vec![Complex32::default(); FFT_LEN],
            scratch,
            cells: vec![Vec::new(); GRID_BINS],
            peak: vec![f32::NEG_INFINITY; GRID_BINS],
            minute: None,
            key: None,
            settled_ms: 0,
        }
    }

    /// Take one frame. The row for the previous minute comes back once a frame
    /// from a later one arrives.
    fn push(&mut self, frame: &Frame) -> Option<Row> {
        let minute = frame.ms / 60_000;
        let done = if self.minute == Some(minute) { None } else { self.minute.replace(minute).and_then(|m| self.finish(m)) };
        if self.key != Some((frame.rate, frame.center_hz)) {
            self.key = Some((frame.rate, frame.center_hz));
            self.settled_ms = frame.ms + SETTLE_MS;
        }
        if frame.ms < self.settled_ms || frame.iq.len() != FFT_LEN {
            return done;
        }
        for ((out, sample), w) in self.buffer.iter_mut().zip(&frame.iq).zip(&self.window) {
            *out = *sample * *w;
        }
        self.fft.process_with_scratch(&mut self.buffer, &mut self.scratch);
        let bin_hz = frame.rate / FFT_LEN as f64;
        let reach = USABLE * frame.rate;
        for (k, value) in self.buffer.iter().enumerate() {
            let signed = if k < FFT_LEN / 2 { k as f64 } else { k as f64 - FFT_LEN as f64 };
            let offset = signed * bin_hz;
            if offset.abs() > reach || offset.abs() < DC_GUARD_HZ {
                continue;
            }
            let cell = ((frame.center_hz + offset) / GRID_HZ).round();
            if cell >= 0.0 && (cell as usize) < GRID_BINS {
                let power = value.norm_sqr() / self.gain;
                let peak = &mut self.peak[cell as usize];
                *peak = peak.max(power);
            }
        }
        for (peak, cell) in self.peak.iter_mut().zip(&mut self.cells) {
            if peak.is_finite() {
                cell.push(quantize(*peak));
                *peak = f32::NEG_INFINITY;
            }
        }
        done
    }

    fn finish(&mut self, minute: u64) -> Option<Row> {
        let enough = |c: &Vec<u8>| c.len() >= MIN_FRAMES;
        let lo = self.cells.iter().position(enough);
        let hi = self.cells.iter().rposition(enough);
        let row = lo.zip(hi).map(|(lo, hi)| {
            let data = self.cells[lo..=hi]
                .iter_mut()
                .map(|cell| {
                    if cell.len() < MIN_FRAMES {
                        return NO_DATA;
                    }
                    let at = (cell.len() - 1) * PERCENTILE / 100;
                    *cell.select_nth_unstable(at).1
                })
                .collect();
            Row { minute, lo, data }
        });
        self.cells.iter_mut().for_each(Vec::clear);
        row
    }
}

/// The radio loop's end: it collects one short run of samples now and then and
/// never waits.
pub struct Tap {
    tx: SyncSender<Frame>,
    collecting: Mutex<Collecting>,
}

#[derive(Default)]
struct Collecting {
    next_ms: u64,
    iq: Vec<Complex32>,
    key: Option<(f64, f64)>,
}

impl Tap {
    pub fn feed(&self, iq: &[Complex32], rate: f64, center_hz: f64, ms: u64) {
        let Ok(mut c) = self.collecting.lock() else { return };
        if c.key != Some((rate, center_hz)) {
            c.key = Some((rate, center_hz));
            c.iq.clear();
        }
        if c.iq.is_empty() && ms < c.next_ms {
            return;
        }
        let need = FFT_LEN - c.iq.len();
        c.iq.extend_from_slice(&iq[..need.min(iq.len())]);
        if c.iq.len() == FFT_LEN {
            c.next_ms = ms + FRAME_EVERY_MS;
            let iq = std::mem::take(&mut c.iq);
            // A full queue costs a frame, and a percentile does not miss one.
            let _ = self.tx.try_send(Frame { iq, rate, center_hz, ms });
        }
    }
}

fn store(row: Row) {
    if let Ok(mut pending) = PENDING.lock() {
        if pending.len() == KEEP_ROWS {
            pending.pop_front();
        }
        pending.push_back(row);
    }
}

pub fn start() -> Option<Tap> {
    let (tx, rx) = sync_channel::<Frame>(QUEUE);
    let worker = std::thread::Builder::new().name("overview".into()).spawn(move || {
        let mut accumulator = Accumulator::new();
        while let Ok(frame) = rx.recv() {
            if let Some(row) = accumulator.push(&frame) {
                store(row);
            }
        }
    });
    if let Err(e) = worker {
        eprintln!("overview: could not start: {e}");
        return None;
    }
    Some(Tap { tx, collecting: Mutex::default() })
}

fn hex(data: &[u8]) -> String {
    use std::fmt::Write;
    data.iter().fold(String::with_capacity(data.len() * 2), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// The oldest rows not yet sent, and the last minute among them.
pub fn pending(limit: usize) -> (Vec<Value>, Option<u64>) {
    let Ok(rows) = PENDING.lock() else { return (Vec::new(), None) };
    let out = rows.iter().take(limit).map(|r| json!({ "m": r.minute, "lo": r.lo, "d": hex(&r.data) })).collect();
    (out, rows.iter().take(limit).last().map(|r| r.minute))
}

/// The backend has everything up to and including this minute.
pub fn sent(through: u64) {
    if let Ok(mut rows) = PENDING.lock() {
        rows.retain(|r| r.minute > through);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 8_000_000.0;
    const CENTER: f64 = 17_000_000.0;
    const NOISE: f32 = 1e-3;

    /// Deterministic noise, with an optional tone at `offset_hz`.
    fn samples(seed: &mut u32, tone: Option<(f64, f32)>) -> Vec<Complex32> {
        let mut next = move |state: &mut u32| {
            *state ^= *state << 13;
            *state ^= *state >> 17;
            *state ^= *state << 5;
            (*state as f32 / u32::MAX as f32 - 0.5) * 2.0 * NOISE
        };
        (0..FFT_LEN)
            .map(|i| {
                let mut s = Complex32::new(next(seed), next(seed));
                if let Some((offset, amp)) = tone {
                    let phase = std::f64::consts::TAU * offset * i as f64 / RATE;
                    s += Complex32::new(phase.cos() as f32, phase.sin() as f32) * amp;
                }
                s
            })
            .collect()
    }

    /// One minute of frames at ten a second, `lit` of them holding the tone.
    fn minute(a: &mut Accumulator, lit: usize, offset: f64) -> Option<Row> {
        let mut seed = 0x1234_5678;
        for i in 0..600 {
            let tone = (i < lit).then_some((offset, 0.05));
            let iq = samples(&mut seed, tone);
            a.push(&Frame { iq, rate: RATE, center_hz: CENTER, ms: 60_000 * 1_000 + i as u64 * 100 });
        }
        a.push(&Frame { iq: samples(&mut seed, None), rate: RATE, center_hz: CENTER, ms: 60_000 * 1_001 })
    }

    fn level(row: &Row, hz: f64) -> u8 {
        row.data[(hz / GRID_HZ).round() as usize - row.lo]
    }

    #[test]
    fn a_steady_tone_lands_in_its_own_cell_and_stands_above_the_rest() {
        let row = minute(&mut Accumulator::new(), 600, 1_000_000.0).unwrap();
        assert_eq!(row.minute, 1_000);
        let (tone, beside) = (level(&row, 18_000_000.0), level(&row, 18_300_000.0));
        assert!(tone as i32 - beside as i32 > 40, "tone {tone}, beside {beside}");
    }

    #[test]
    fn a_two_second_burst_shows_and_a_click_does_not() {
        let burst = minute(&mut Accumulator::new(), 20, 1_000_000.0).unwrap();
        assert!(level(&burst, 18_000_000.0) as i32 - level(&burst, 18_300_000.0) as i32 > 40);
        let click = minute(&mut Accumulator::new(), 2, 1_000_000.0).unwrap();
        assert!((level(&click, 18_000_000.0) as i32 - level(&click, 18_300_000.0) as i32).abs() < 10);
    }

    #[test]
    fn only_the_usable_span_is_reported_and_center_is_left_out() {
        let row = minute(&mut Accumulator::new(), 0, 0.0).unwrap();
        let reach = USABLE * RATE;
        assert_eq!(row.lo, ((CENTER - reach) / GRID_HZ).ceil() as usize);
        assert_eq!(row.lo + row.data.len() - 1, ((CENTER + reach) / GRID_HZ).floor() as usize);
        assert_eq!(level(&row, CENTER), NO_DATA);
    }

    #[test]
    fn a_tuner_that_passed_through_briefly_reports_nothing() {
        let mut a = Accumulator::new();
        let mut seed = 7;
        for i in 0..30 {
            a.push(&Frame { iq: samples(&mut seed, None), rate: RATE, center_hz: CENTER, ms: 60_000_000 + i * 100 });
        }
        assert_eq!(a.push(&Frame { iq: samples(&mut seed, None), rate: RATE, center_hz: CENTER, ms: 60_060_000 }), None);
    }

    #[test]
    fn frames_right_after_a_retune_are_skipped() {
        let mut a = Accumulator::new();
        let mut seed = 9;
        for i in 0..4 {
            a.push(&Frame { iq: samples(&mut seed, None), rate: RATE, center_hz: CENTER, ms: 60_000_000 + i * 100 });
        }
        assert!(a.cells.iter().all(Vec::is_empty));
        a.push(&Frame { iq: samples(&mut seed, None), rate: RATE, center_hz: CENTER, ms: 60_000_600 });
        assert!(a.cells.iter().any(|c| c.len() == 1));
    }

    #[test]
    fn the_tap_hands_over_whole_contiguous_frames_at_its_own_pace() {
        let (tx, rx) = sync_channel(4);
        let tap = Tap { tx, collecting: Mutex::default() };
        let block = vec![Complex32::default(); 1_500];
        for i in 0..3 {
            tap.feed(&block, RATE, CENTER, 1_000 + i);
        }
        let frame = rx.try_recv().unwrap();
        assert_eq!(frame.iq.len(), FFT_LEN);
        tap.feed(&block, RATE, CENTER, 1_050);
        assert!(rx.try_recv().is_err(), "too soon for another frame");
        assert!(tap.collecting.lock().unwrap().iq.is_empty());
    }
}
