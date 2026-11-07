//! The streaming front end: sideband audio in at whatever rate the channel
//! runs, UTC slots of every speed asked for out, decoded on a worker
//! thread, and frames joined into messages.
//!
//! The clock is kept as FT8's (`ft8::stream`) keeps it: audio arrives in
//! blocks stamped with the wall-clock time of their last sample, every
//! 12 kHz output sample is given a time by counting samples, and the stamps
//! are averaged into how far that clock has drifted. With several slot
//! lengths running at once, the drift is taken up only where all their
//! slots begin together (every 30 s for the four speeds), in the quiet
//! between transmissions, so no slot is ever cut with a jump inside it.
//! One resampler feeds every speed; each speed counts its own slots.

use super::assemble::Assembler;
use super::{F_LIMIT_HZ, F_MAX_HZ, F_MIN_HZ, Js8Event, Js8Frame, Js8Speed, RATE_HZ, decode_slot_in};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::Arc;

/// Anti-alias filter length at the input rate: at 48 kHz, flat to about
/// 4.5 kHz, so a search out to the widest 5 kHz filter is not tilted.
const TAPS: usize = 127;
/// Half amplitude here; 12 kHz's 6 kHz Nyquist then folds only what the
/// sideband filter has already removed.
const CUTOFF_HZ: f64 = 5_200.0;
/// A stamp further than this from the sample clock means audio went
/// missing (or the clock stepped), not jitter.
const RESYNC_MS: f64 = 250.0;
/// A slot with less than this share of its audio is not worth decoding.
const MIN_FILL: f64 = 0.6;

struct SlotBuf {
    audio: Vec<f32>,
    filled: usize,
}

/// One speed's slots.
struct Lane {
    speed: Js8Speed,
    slot_ms: u64,
    slot_len: usize,
    /// The next sample's place: (slot start, index in it), counted rather
    /// than recomputed from the clock, for the reason `ft8::stream` gives.
    cursor: Option<(u64, usize)>,
    slots: BTreeMap<u64, SlotBuf>,
}

struct Job {
    speed: Js8Speed,
    start: u64,
    audio: Vec<f32>,
    band: (f32, f32),
}

/// What the decoder is doing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Js8Status {
    pub speeds: Vec<Js8Speed>,
    /// The audio searched, Hz.
    pub band: (f32, f32),
    /// Slots waiting for or in the worker.
    pub queued: usize,
    /// Slots skipped because the worker was a round behind.
    pub dropped: u64,
    /// Times the stamps disagreed with the sample clock by more than
    /// jitter, and the clock was set to them.
    pub resyncs: u64,
    pub slots_decoded: u64,
    pub frames: u64,
    pub messages: u64,
    /// Transmissions with frames still to come.
    pub open_messages: usize,
    /// The start of the last slot decoded, UNIX ms, and how long it took.
    pub last_slot_ms: Option<u64>,
    pub last_decode_ms: Option<f32>,
}

pub struct Js8Decoder {
    input_rate: f64,
    taps: Vec<f32>,
    /// Raw input, `hist[0]` being input sample `hist_start`.
    hist: Vec<f32>,
    hist_start: u64,
    in_count: u64,
    /// Input position, fractional, of the next 12 kHz sample.
    next_pos: f64,
    /// Input sample index and the UNIX ms it was heard at.
    anchor: Option<(u64, f64)>,
    /// How far the stamps run ahead of the sample clock, ms, averaged.
    drift_ms: f64,
    /// Slots of every speed begin together on multiples of this, and the
    /// period the last sample fell in.
    epoch_ms: u64,
    epoch: Option<u64>,
    lanes: Vec<Lane>,
    band: (f32, f32),
    jobs: Sender<Job>,
    results: Receiver<(Js8Speed, u64, Vec<Js8Frame>, f32)>,
    queued: Arc<AtomicUsize>,
    dropped: Arc<AtomicU64>,
    resyncs: u64,
    assembler: Assembler,
    stats: Js8Status,
    /// Open transmissions to report at the next take, after a reset.
    flush_pending: bool,
    /// Ends once the job sender is dropped with the decoder.
    _worker: Option<std::thread::JoinHandle<()>>,
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

impl Js8Decoder {
    /// A decoder for audio at `fs` Hz (any rate, not only round ones),
    /// searching every speed in `speeds` at once; Normal alone if empty.
    pub fn new(fs: f32, speeds: &[Js8Speed]) -> Self {
        let input_rate = fs as f64;
        let fc = CUTOFF_HZ / input_rate;
        let mid = (TAPS / 2) as f64;
        let mut taps: Vec<f32> = (0..TAPS)
            .map(|i| {
                let x = i as f64 - mid;
                let sinc = if x == 0.0 { 2.0 * fc } else { (std::f64::consts::TAU * fc * x).sin() / (std::f64::consts::PI * x) };
                let a = std::f64::consts::TAU * i as f64 / (TAPS - 1) as f64;
                (sinc * (0.42 - 0.5 * a.cos() + 0.08 * (2.0 * a).cos())) as f32
            })
            .collect();
        let sum: f32 = taps.iter().sum();
        taps.iter_mut().for_each(|t| *t /= sum);
        let mut speeds: Vec<Js8Speed> = speeds.to_vec();
        speeds.sort();
        speeds.dedup();
        if speeds.is_empty() {
            speeds.push(Js8Speed::Normal);
        }
        let lanes: Vec<Lane> = speeds
            .iter()
            .map(|&speed| Lane {
                speed,
                slot_ms: (speed.slot_secs() * 1000.0) as u64,
                slot_len: (speed.slot_secs() * RATE_HZ as f64) as usize,
                cursor: None,
                slots: BTreeMap::new(),
            })
            .collect();
        let epoch_ms = lanes.iter().fold(1, |l, lane| l / gcd(l, lane.slot_ms) * lane.slot_ms);
        let (jobs, job_rx) = channel::<Job>();
        let (res_tx, results) = channel();
        let queued = Arc::new(AtomicUsize::new(0));
        let q = queued.clone();
        // One worker for every speed: slots that end together are decoded
        // one after another rather than all at once on a small host.
        let worker = std::thread::Builder::new()
            .name("js8-decode".into())
            .spawn(move || {
                for job in job_rx {
                    let t = std::time::Instant::now();
                    let mut frames = decode_slot_in(job.speed, &job.audio, job.band.0, job.band.1);
                    for f in &mut frames {
                        f.slot_ms = job.start;
                    }
                    let took = t.elapsed().as_secs_f32() * 1000.0;
                    q.fetch_sub(1, Ordering::AcqRel);
                    if res_tx.send((job.speed, job.start, frames, took)).is_err() {
                        break;
                    }
                }
            })
            .ok();
        let stats = Js8Status { speeds: speeds.clone(), band: (F_MIN_HZ, F_MAX_HZ), ..Default::default() };
        Js8Decoder {
            input_rate,
            taps,
            hist: Vec::new(),
            hist_start: 0,
            in_count: 0,
            next_pos: (TAPS / 2) as f64,
            anchor: None,
            drift_ms: 0.0,
            epoch_ms,
            epoch: None,
            lanes,
            band: (F_MIN_HZ, F_MAX_HZ),
            jobs,
            results,
            queued,
            dropped: Arc::new(AtomicU64::new(0)),
            resyncs: 0,
            assembler: Assembler::default(),
            stats,
            flush_pending: false,
            _worker: worker,
        }
    }

    pub fn speeds(&self) -> Vec<Js8Speed> {
        self.lanes.iter().map(|l| l.speed).collect()
    }

    /// Search `low_hz..high_hz` of the audio: the receiver's passband.
    /// Slots already cut keep the band they were cut with.
    pub fn set_band(&mut self, low_hz: f32, high_hz: f32) {
        let high = high_hz.clamp(300.0, F_LIMIT_HZ);
        self.band = (low_hz.clamp(0.0, high - 200.0), high);
    }

    pub fn band(&self) -> (f32, f32) {
        self.band
    }

    /// Forget the audio in hand (after a retune or a gap). Transmissions
    /// still open are reported, incomplete, by the next [`take`](Self::take).
    pub fn reset(&mut self) {
        self.hist.clear();
        self.hist_start = 0;
        self.in_count = 0;
        self.next_pos = (TAPS / 2) as f64;
        self.anchor = None;
        self.drift_ms = 0.0;
        self.epoch = None;
        for lane in &mut self.lanes {
            lane.cursor = None;
            lane.slots.clear();
        }
        self.flush_pending = true;
    }

    /// Audio at the input rate; `end_ms` is the UNIX time of its last
    /// sample, as `ft8::SlotDecoder` takes it.
    pub fn push(&mut self, samples: &[f32], end_ms: u64) {
        if samples.is_empty() {
            return;
        }
        self.hist.extend_from_slice(samples);
        self.in_count += samples.len() as u64;
        let last = self.in_count - 1;
        let ms_per = 1000.0 / self.input_rate;
        let stamp = end_ms as f64;
        match self.anchor {
            Some((i, t)) => {
                let err = stamp - (t + (last - i) as f64 * ms_per);
                if err.abs() > RESYNC_MS {
                    self.resyncs += 1;
                    self.anchor = Some((last, stamp));
                    self.drift_ms = 0.0;
                } else {
                    self.drift_ms += (err - self.drift_ms) * 0.02;
                }
            }
            None => self.anchor = Some((last, stamp)),
        }
        let step = self.input_rate / RATE_HZ as f64;
        let half = TAPS / 2;
        let mut latest = f64::NEG_INFINITY;
        while (self.next_pos.floor() as u64) + 1 + half as u64 <= last {
            let i = self.next_pos.floor() as u64;
            let frac = (self.next_pos - i as f64) as f32;
            let y0 = self.filtered(i);
            let y1 = self.filtered(i + 1);
            let y = y0 + (y1 - y0) * frac;
            let (ai, at) = self.anchor.expect("set above");
            let mut t = at + (self.next_pos - ai as f64) * ms_per;
            self.next_pos += step;
            if t < 0.0 {
                continue;
            }
            // Where every speed's slots begin together: the one place the
            // clock takes up its drift.
            if self.epoch != Some((t as u64 / self.epoch_ms) * self.epoch_ms) {
                self.anchor = Some((ai, at + self.drift_ms));
                t += self.drift_ms;
                self.drift_ms = 0.0;
                self.epoch = Some((t.max(0.0) as u64 / self.epoch_ms) * self.epoch_ms);
            }
            latest = t;
            for lane in &mut self.lanes {
                let start = (t as u64 / lane.slot_ms) * lane.slot_ms;
                let exact = (t - start as f64) * RATE_HZ as f64 / 1000.0;
                let idx = match lane.cursor {
                    Some((s, i)) if s == start && (i as f64 - exact).abs() < 0.75 => i,
                    _ => exact.round() as usize,
                };
                lane.cursor = Some((start, idx + 1));
                if idx >= lane.slot_len {
                    continue;
                }
                let len = lane.slot_len;
                let buf = lane.slots.entry(start).or_insert_with(|| SlotBuf { audio: vec![0.0; len], filled: 0 });
                buf.audio[idx] = y;
                buf.filled += 1;
            }
        }
        let keep_from = (self.next_pos.floor() as u64).saturating_sub(half as u64 + 1);
        if keep_from > self.hist_start {
            let drop = ((keep_from - self.hist_start) as usize).min(self.hist.len());
            self.hist.drain(..drop);
            self.hist_start += drop as u64;
        }
        if latest.is_finite() {
            self.finish_before(latest);
        }
    }

    fn filtered(&self, i: u64) -> f32 {
        let half = TAPS / 2;
        let base = i as i64 - half as i64 - self.hist_start as i64;
        let mut acc = 0.0;
        for (k, h) in self.taps.iter().enumerate() {
            let n = base + k as i64;
            if n >= 0
                && let Some(x) = self.hist.get(n as usize)
            {
                acc += h * x;
            }
        }
        acc
    }

    /// Cut every slot that ended before `now_ms` and queue it.
    fn finish_before(&mut self, now_ms: f64) {
        let mut cut = Vec::new();
        for lane in &mut self.lanes {
            let done: Vec<u64> = lane.slots.keys().copied().filter(|&s| (s + lane.slot_ms) as f64 <= now_ms).collect();
            for start in done {
                let Some(buf) = lane.slots.remove(&start) else { continue };
                if (buf.filled as f64) >= MIN_FILL * lane.slot_len as f64 {
                    cut.push(Job { speed: lane.speed, start, audio: buf.audio, band: self.band });
                }
            }
        }
        // Slots of different speeds that end together go shortest first:
        // a Turbo frame is worth less the later it comes.
        cut.sort_by_key(|j| (j.start + (j.speed.slot_secs() * 1000.0) as u64, (j.speed.slot_secs() * 1000.0) as u64));
        for job in cut {
            // A round of every speed may wait; more than that means the
            // host cannot keep up, and a late slot is worth less than the
            // next one on time.
            if self.queued.load(Ordering::Acquire) > self.lanes.len() {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            self.queued.fetch_add(1, Ordering::AcqRel);
            if self.jobs.send(job).is_err() {
                self.queued.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    /// Whether slots are waiting for or in the worker.
    pub fn busy(&self) -> bool {
        self.queued.load(Ordering::Acquire) > 0
    }

    /// Events since the last call: every frame decoded, then the
    /// transmissions they finished.
    pub fn take(&mut self) -> Vec<Js8Event> {
        let mut out = Vec::new();
        if std::mem::take(&mut self.flush_pending) {
            for m in self.assembler.flush() {
                self.stats.messages += 1;
                out.push(Js8Event::Message(m));
            }
        }
        let mut done: Vec<(Js8Speed, u64, Vec<Js8Frame>, f32)> = self.results.try_iter().collect();
        done.sort_by_key(|(s, start, ..)| (*start, *s));
        for (speed, start, frames, took) in done {
            self.stats.slots_decoded += 1;
            self.stats.frames += frames.len() as u64;
            self.stats.last_slot_ms = Some(start);
            self.stats.last_decode_ms = Some(took);
            let messages = self.assembler.slot(speed, start, &frames);
            out.extend(frames.into_iter().map(Js8Event::Frame));
            self.stats.messages += messages.len() as u64;
            out.extend(messages.into_iter().map(Js8Event::Message));
        }
        out
    }

    pub fn status(&self) -> Js8Status {
        Js8Status {
            band: self.band,
            queued: self.queued.load(Ordering::Acquire),
            dropped: self.dropped.load(Ordering::Relaxed),
            resyncs: self.resyncs,
            open_messages: self.assembler.open(),
            ..self.stats.clone()
        }
    }
}
