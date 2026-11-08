//! The streaming front end: sideband audio in at whatever rate the channel
//! runs, finished slots of each T/R period out, decoded off the caller's
//! thread.
//!
//! Audio is mixed to the 375 Hz baseband once; every enabled period keeps
//! its own slot buffer and its own wall clock, so that the clock of a
//! 30-minute slot is left alone by what a 2-minute slot does. As in the WSPR
//! front end, baseband samples are placed by a sample-counting clock whose
//! drift from the stamps is taken up only where that period's slot begins.
//! Slots from different periods end at the same moment on the hour, so they
//! are decoded one at a time by a worker over a short queue, and a slot is
//! dropped only if that queue is already full.

use super::{decode_baseband, CallHashes, Fst4wDecode, Period, BASEBAND_HZ};
use crate::wspr::Downconverter;
use num_complex::Complex32;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

/// A stamp further than this from the sample clock means audio went
/// missing, not jitter.
const RESYNC_MS: f64 = 250.0;
/// A slot with less than this share of its audio is not worth decoding: a
/// frame fills nearly the whole slot.
const MIN_FILL: f64 = 0.85;
/// Slots waiting for the worker; more than this and the newest is dropped.
const MAX_QUEUE: usize = 6;

struct SlotBuf {
    z: Vec<Complex32>,
    filled: usize,
}

/// One period's slot state.
struct Slicer {
    period: Period,
    anchor: Option<(u64, f64)>,
    drift_ms: f64,
    current: Option<u64>,
    cursor: Option<(u64, usize)>,
    slots: BTreeMap<u64, SlotBuf>,
    last_cut: Option<u64>,
}

impl Slicer {
    fn new(period: Period) -> Self {
        Slicer { period, anchor: None, drift_ms: 0.0, current: None, cursor: None, slots: BTreeMap::new(), last_cut: None }
    }

    fn reset(&mut self) {
        self.anchor = None;
        self.drift_ms = 0.0;
        self.current = None;
        self.cursor = None;
        self.slots.clear();
    }

    /// Note a stamp; true when it disagreed by enough to reset the clock.
    fn stamp(&mut self, first: u64, stamp: f64, ms_per: f64) -> bool {
        let mut resync = false;
        match self.anchor {
            Some((i, t)) => {
                let err = stamp - (t + (first - i) as f64 * ms_per);
                if err.abs() > RESYNC_MS {
                    resync = true;
                    self.anchor = Some((first, stamp));
                    self.drift_ms = 0.0;
                } else {
                    self.drift_ms += (err - self.drift_ms) * 0.02;
                }
            }
            None => self.anchor = Some((first, stamp)),
        }
        resync
    }

    fn accept(&mut self, pos: f64, v: Complex32, ms_per: f64) -> Option<f64> {
        let slot_ms = self.period.slot_ms();
        let keep = (self.period.keep_secs() as f32 * BASEBAND_HZ) as usize;
        let (ai, at) = self.anchor?;
        let mut t = at + (pos - ai as f64) * ms_per;
        if t < 0.0 {
            return None;
        }
        if self.current != Some((t as u64 / slot_ms) * slot_ms) {
            self.anchor = Some((ai, at + self.drift_ms));
            t += self.drift_ms;
            self.drift_ms = 0.0;
            self.current = Some((t.max(0.0) as u64 / slot_ms) * slot_ms);
        }
        let start = (t as u64 / slot_ms) * slot_ms;
        let exact = (t - start as f64) * BASEBAND_HZ as f64 / 1000.0;
        let idx = match self.cursor {
            Some((s, i)) if s == start && (i as f64 - exact).abs() < 0.75 => i,
            _ => exact.round() as usize,
        };
        self.cursor = Some((start, idx + 1));
        if idx < keep && self.last_cut.is_none_or(|c| start > c) {
            let buf = self.slots.entry(start).or_insert_with(|| SlotBuf { z: vec![Complex32::new(0.0, 0.0); keep], filled: 0 });
            buf.z[idx] = v;
            buf.filled += 1;
        }
        Some(t)
    }
}

struct Job {
    period: Period,
    start: u64,
    z: Vec<Complex32>,
}

struct Shared {
    queue: Mutex<VecDeque<Job>>,
    running: AtomicBool,
    hashes: Mutex<CallHashes>,
    tx: Mutex<Sender<(u64, Vec<Fst4wDecode>)>>,
    dropped: AtomicU64,
    last_count: AtomicI64,
    last_slot: AtomicI64,
}

/// Where the decoder is in the slot cycle.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Fst4wStatus {
    /// Seconds into the current slot of the shortest enabled period, by the
    /// audio's clock; `None` before any audio.
    pub slot_secs: Option<f32>,
    /// The shortest period's audio is being kept.
    pub collecting: bool,
    /// A finished slot is being decoded, or waits.
    pub decoding: bool,
    /// Decodes in the last slot decoded, and that slot's start (UNIX ms).
    pub last_count: Option<usize>,
    pub last_slot_ms: Option<u64>,
}

/// FST4W from a stream of audio: push audio as it arrives, take decodes as
/// slots finish. By default all four periods decode at once.
pub struct Fst4wDecoder {
    input_rate: f64,
    dc: Downconverter,
    slicers: Vec<Slicer>,
    shared: Arc<Shared>,
    rx: Receiver<(u64, Vec<Fst4wDecode>)>,
    latest_ms: Option<f64>,
    resyncs: u64,
    out: Vec<(f64, Complex32)>,
}

impl Fst4wDecoder {
    /// Decode every period from audio at `fs` Hz.
    pub fn new(fs: f32) -> Self {
        Self::with_periods(fs, &Period::ALL)
    }

    /// Decode only the periods named.
    pub fn with_periods(fs: f32, periods: &[Period]) -> Self {
        let (tx, rx) = channel();
        let mut ps = periods.to_vec();
        ps.sort();
        ps.dedup();
        Fst4wDecoder {
            input_rate: fs as f64,
            dc: Downconverter::new(fs),
            slicers: ps.into_iter().map(Slicer::new).collect(),
            shared: Arc::new(Shared {
                queue: Mutex::new(VecDeque::new()),
                running: AtomicBool::new(false),
                hashes: Mutex::new(CallHashes::default()),
                tx: Mutex::new(tx),
                dropped: AtomicU64::new(0),
                last_count: AtomicI64::new(-1),
                last_slot: AtomicI64::new(-1),
            }),
            rx,
            latest_ms: None,
            resyncs: 0,
            out: Vec::new(),
        }
    }

    /// Slots dropped because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    /// Times a stamp disagreed with the sample clock by more than jitter.
    pub fn resyncs(&self) -> u64 {
        self.resyncs
    }

    /// Calls heard in full that type 3 hashes can name.
    pub fn known_calls(&self) -> usize {
        self.shared.hashes.lock().map(|h| h.len()).unwrap_or(0)
    }

    /// Forget the audio in hand (after a retune or a gap). Calls heard so
    /// far are kept.
    pub fn reset(&mut self) {
        self.dc = Downconverter::new(self.input_rate as f32);
        self.latest_ms = None;
        self.slicers.iter_mut().for_each(Slicer::reset);
    }

    /// Audio and the wall-clock time (ms since the Unix epoch) of its first
    /// sample.
    pub fn push(&mut self, audio: &[f32], t0_unix_ms: u64) {
        if audio.is_empty() {
            return;
        }
        let first = self.dc.consumed();
        let ms_per = 1000.0 / self.input_rate;
        let stamp = t0_unix_ms as f64;
        for s in &mut self.slicers {
            if s.stamp(first, stamp, ms_per) {
                self.resyncs += 1;
            }
        }
        let mut out = std::mem::take(&mut self.out);
        out.clear();
        self.dc.push(audio, &mut out);
        for &(pos, v) in &out {
            for s in &mut self.slicers {
                if let Some(t) = s.accept(pos, v, ms_per) {
                    self.latest_ms = Some(self.latest_ms.map_or(t, |l| l.max(t)));
                }
            }
        }
        self.out = out;
        if let Some(now) = self.latest_ms {
            self.finish_before(now);
        }
    }

    fn finish_before(&mut self, now_ms: f64) {
        for s in &mut self.slicers {
            let keep_ms = s.period.keep_secs() as f64 * 1000.0;
            let keep = (s.period.keep_secs() as f32 * BASEBAND_HZ) as usize;
            let done: Vec<u64> = s.slots.keys().copied().filter(|&k| k as f64 + keep_ms <= now_ms).collect();
            for start in done {
                let Some(buf) = s.slots.remove(&start) else { continue };
                s.last_cut = Some(s.last_cut.map_or(start, |c| c.max(start)));
                if (buf.filled as f64) < MIN_FILL * keep as f64 {
                    continue;
                }
                enqueue(&self.shared, Job { period: s.period, start, z: buf.z });
            }
        }
    }

    /// Decodes from finished slots: (slot start, UNIX ms; decodes). Each
    /// decode carries its period.
    pub fn take(&mut self) -> Vec<(u64, Vec<Fst4wDecode>)> {
        self.rx.try_iter().collect()
    }

    pub fn status(&self) -> Fst4wStatus {
        let shortest = self.slicers.first().map(|s| s.period);
        let slot_secs = shortest.and_then(|p| self.latest_ms.map(|t| ((t as u64 % p.slot_ms()) as f64 / 1000.0) as f32));
        let keep = shortest.map_or(0.0, |p| p.keep_secs() as f32);
        let queued = self.shared.queue.lock().map(|q| q.len()).unwrap_or(0);
        let count = self.shared.last_count.load(Ordering::Relaxed);
        let slot = self.shared.last_slot.load(Ordering::Relaxed);
        Fst4wStatus {
            slot_secs,
            collecting: slot_secs.is_some_and(|s| s < keep),
            decoding: self.shared.running.load(Ordering::Relaxed) || queued > 0,
            last_count: (count >= 0).then_some(count as usize),
            last_slot_ms: (slot >= 0).then_some(slot as u64),
        }
    }
}

fn enqueue(shared: &Arc<Shared>, job: Job) {
    {
        let mut q = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() >= MAX_QUEUE {
            shared.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        q.push_back(job);
    }
    if shared.running.swap(true, Ordering::AcqRel) {
        return;
    }
    let s = shared.clone();
    let spawned = std::thread::Builder::new().name("fst4w-decode".into()).spawn(move || loop {
        let job = {
            let mut q = s.queue.lock().unwrap_or_else(|e| e.into_inner());
            match q.pop_front() {
                Some(j) => j,
                None => {
                    // Checked and cleared under the lock, so a job pushed
                    // right now either was seen above or starts a worker.
                    s.running.store(false, Ordering::Release);
                    return;
                }
            }
        };
        let decodes = {
            let mut h = s.hashes.lock().unwrap_or_else(|e| e.into_inner());
            decode_baseband(&job.z, job.period, &mut h)
        };
        s.last_count.store(decodes.len() as i64, Ordering::Relaxed);
        s.last_slot.store(job.start as i64, Ordering::Relaxed);
        if let Ok(tx) = s.tx.lock() {
            let _ = tx.send((job.start, decodes));
        }
    });
    if spawned.is_err() {
        shared.running.store(false, Ordering::Release);
    }
}
