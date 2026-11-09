//! The streaming front end: sideband audio in at whatever rate the channel
//! runs, UTC-aligned slots out, each decoded off the caller's thread.
//!
//! Audio arrives in blocks stamped with the wall-clock time of their last
//! sample. Every 12 kHz output sample is given a time from a clock kept by
//! counting samples. The stamps (which jitter with delivery) are averaged
//! into how far that clock has drifted, and the drift is taken up only where
//! a new slot begins, in the quiet between transmissions: steering the clock
//! within a slot moved samples by fractions of a millisecond many times a
//! second, a phase jump that grows with audio frequency, and on the air it
//! left only the signals below about 500 Hz decodable. The clock is reset
//! outright only when a stamp disagrees by more than a gap would explain.
//! Samples land in the slot their time falls in; a slot is cut once audio
//! past its end has arrived.

use super::{CallHashes, Decode, F_LIMIT_HZ, F_MAX_HZ, F_MIN_HZ, Options, Protocol, RATE_HZ};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Anti-alias filter length at the input rate: at 48 kHz, flat (within
/// 0.1 dB) to about 4.5 kHz, so a search out to the widest 5 kHz filter
/// is not tilted by the resampler.
const TAPS: usize = 127;
/// Half amplitude here; the rate then falls to 12 kHz, whose 6 kHz Nyquist
/// folds only what the sideband filter has already removed.
const CUTOFF_HZ: f64 = 5_200.0;
/// A stamp further than this from the sample clock means audio went
/// missing (or the clock stepped), not jitter.
const RESYNC_MS: f64 = 250.0;
/// A slot with less than this share of its audio is not worth decoding.
const MIN_FILL: f64 = 0.6;

/// The share of its slot period a decode should be done within, counted
/// from when the slot was cut: the next slot is already filling.
const DUE: f64 = 0.7;
/// The most one decode's finer passes are given, however idle the host.
const BUDGET_MAX: Duration = Duration::from_millis(4_000);

struct SlotBuf {
    audio: Vec<f32>,
    filled: usize,
}

/// A slot waiting to be decoded.
struct Job {
    protocol: Protocol,
    start: u64,
    audio: Vec<f32>,
    band: (f32, f32),
    cut: Instant,
    busy: Arc<AtomicBool>,
    tx: Sender<(u64, Vec<Decode>)>,
}

/// The workers every slot decoder shares. Every channel's slots end at the
/// same instant; a thread each meant as many decodes at once as there were
/// channels, on a host with two cores that also has a radio to read, and
/// each decode's deadline ran out with a fraction of its work done. Here
/// they wait their turn, and each is told how long it has from how much of
/// the slot period is left and how many are behind it.
struct Pool {
    jobs: Mutex<Sender<Job>>,
    waiting: Arc<AtomicU64>,
    workers: u32,
}

/// The calls heard in full, kept by every channel together: a station
/// named on one band is often sent as a hash on another.
fn calls() -> &'static Mutex<CallHashes> {
    static CALLS: OnceLock<Mutex<CallHashes>> = OnceLock::new();
    CALLS.get_or_init(Default::default)
}

/// Put the calling thread below the threads that carry the radio's samples.
/// A decode is a burst of seconds of arithmetic, and nothing waits on it.
fn lower_priority() {
    #[cfg(target_os = "linux")]
    // SAFETY: plain system calls on the calling thread's own id.
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, 10);
    }
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| {
        let (tx, rx) = channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        let waiting = Arc::new(AtomicU64::new(0));
        // One core is left for the radio.
        let workers = std::thread::available_parallelism().map_or(1, |n| n.get().saturating_sub(1)).clamp(1, 3) as u32;
        for w in 0..workers {
            let (rx, waiting) = (rx.clone(), waiting.clone());
            let _ = std::thread::Builder::new().name(format!("slot-decode-{w}")).spawn(move || {
                lower_priority();
                loop {
                    let job = match rx.lock().unwrap_or_else(|e| e.into_inner()).recv() {
                        Ok(job) => job,
                        Err(_) => return,
                    };
                    let behind = waiting.fetch_sub(1, Ordering::AcqRel).saturating_sub(1);
                    let due = job.cut + Duration::from_secs_f64(job.protocol.slot_secs() * DUE);
                    let left = due.saturating_duration_since(Instant::now());
                    // This decode's share of what is left, the workers
                    // taking the queue between them.
                    let budget = left.mul_f64(f64::from(workers) / (behind as f64 + f64::from(workers))).min(BUDGET_MAX);
                    let options = Options::default().with_budget(budget);
                    // A panic in a decode must not leave its channel marked
                    // busy for good, dropping every later slot.
                    let decodes = std::panic::catch_unwind(|| {
                        let heard = super::find(job.protocol, &job.audio, job.band.0, job.band.1, &options);
                        let mut calls = calls().lock().unwrap_or_else(|e| e.into_inner());
                        super::name(job.protocol, heard, &mut calls)
                    })
                    .unwrap_or_default();
                    let _ = job.tx.send((job.start, decodes));
                    job.busy.store(false, Ordering::Release);
                }
            });
        }
        Pool { jobs: Mutex::new(tx), waiting, workers }
    })
}

pub struct SlotDecoder {
    protocol: Protocol,
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
    /// How far the stamps run ahead of the sample clock, ms, averaged; taken
    /// into the anchor when a slot begins.
    drift_ms: f64,
    /// The slot the last sample went to.
    current: Option<u64>,
    /// The next sample's place: (slot start, index in it). Outputs are one
    /// 12 kHz sample apart, so the index is counted, not recomputed from
    /// the clock: truncating a float time put a sample on the clock's
    /// fraction-of-a-sample edge into the neighbor's place, leaving a
    /// duplicate and a hole, and the slot lost its top octave (a 4 kHz tone
    /// came out 10 dB down).
    cursor: Option<(u64, usize)>,
    slots: BTreeMap<u64, SlotBuf>,
    busy: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    /// Times the sample clock was thrown away for a stamp too far off.
    resyncs: u64,
    /// Where each cut slot's 12 kHz audio is written (f32 LE), when set.
    dump: Option<std::path::PathBuf>,
    /// The audio searched for signals, Hz.
    band: (f32, f32),
    tx: Sender<(u64, Vec<Decode>)>,
    rx: Receiver<(u64, Vec<Decode>)>,
}

impl SlotDecoder {
    pub fn new(protocol: Protocol, input_rate_hz: f64) -> Self {
        let fc = CUTOFF_HZ / input_rate_hz;
        let mid = (TAPS / 2) as f64;
        let mut taps: Vec<f32> = (0..TAPS)
            .map(|i| {
                let x = i as f64 - mid;
                let sinc = if x == 0.0 { 2.0 * fc } else { (std::f64::consts::TAU * fc * x).sin() / (std::f64::consts::PI * x) };
                let w = 0.42 - 0.5 * (std::f64::consts::TAU * i as f64 / (TAPS - 1) as f64).cos() + 0.08 * (2.0 * std::f64::consts::TAU * i as f64 / (TAPS - 1) as f64).cos();
                (sinc * w) as f32
            })
            .collect();
        let sum: f32 = taps.iter().sum();
        taps.iter_mut().for_each(|t| *t /= sum);
        let (tx, rx) = channel();
        SlotDecoder {
            protocol,
            input_rate: input_rate_hz,
            taps,
            hist: Vec::new(),
            hist_start: 0,
            in_count: 0,
            next_pos: (TAPS / 2) as f64,
            anchor: None,
            drift_ms: 0.0,
            current: None,
            cursor: None,
            slots: BTreeMap::new(),
            busy: Arc::new(AtomicBool::new(false)),
            dropped: Arc::new(AtomicU64::new(0)),
            resyncs: 0,
            dump: None,
            band: (F_MIN_HZ, F_MAX_HZ),
            tx,
            rx,
        }
    }

    pub fn protocol(&self) -> Protocol {
        self.protocol
    }

    /// Slots skipped because the previous one was still decoding.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Times the stamps disagreed with the sample clock by more than a
    /// delivery's jitter, and the clock was set to them.
    pub fn resyncs(&self) -> u64 {
        self.resyncs
    }

    /// Write every slot cut to `dir` as `<slot start ms>.f32`, raw 12 kHz
    /// little-endian floats, for decoding the same audio again offline.
    pub fn set_dump(&mut self, dir: Option<std::path::PathBuf>) {
        self.dump = dir;
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

    /// Whether a slot is being decoded right now.
    pub fn busy(&self) -> bool {
        self.busy.load(Ordering::Relaxed)
    }

    /// Forget the audio in hand (after a retune or a gap). Calls heard so
    /// far are kept: the band has not changed its stations.
    pub fn reset(&mut self) {
        self.hist.clear();
        self.hist_start = 0;
        self.in_count = 0;
        self.next_pos = (TAPS / 2) as f64;
        self.anchor = None;
        self.drift_ms = 0.0;
        self.current = None;
        self.cursor = None;
        self.slots.clear();
    }

    /// Audio at the input rate; `end_ms` is the UNIX time of its last sample.
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
        let slot_ms = (self.protocol.slot_secs() * 1000.0) as u64;
        let slot_len = (self.protocol.slot_secs() * RATE_HZ as f64) as usize;
        let mut latest = f64::NEG_INFINITY;
        // Each output needs the filter centered on two input samples.
        while (self.next_pos.floor() as u64) + 1 + half as u64 <= last {
            let i = self.next_pos.floor() as u64;
            let frac = (self.next_pos - i as f64) as f32;
            let y0 = self.filtered(i);
            let y1 = self.filtered(i + 1);
            let y = y0 + (y1 - y0) * frac;
            let (ai, at) = self.anchor.expect("set above");
            let mut t = at + (self.next_pos - ai as f64) * ms_per;
            if t < 0.0 {
                self.next_pos += step;
                continue;
            }
            // A new slot: the one place the clock takes up its drift.
            if self.current != Some((t as u64 / slot_ms) * slot_ms) {
                self.anchor = Some((ai, at + self.drift_ms));
                t += self.drift_ms;
                self.drift_ms = 0.0;
                self.current = Some((t.max(0.0) as u64 / slot_ms) * slot_ms);
            }
            self.next_pos += step;
            latest = t;
            let start = (t as u64 / slot_ms) * slot_ms;
            let exact = (t - start as f64) * RATE_HZ as f64 / 1000.0;
            let idx = match self.cursor {
                // Within a slot, while the count agrees with the clock.
                Some((s, i)) if s == start && (i as f64 - exact).abs() < 0.75 => i,
                _ => exact.round() as usize,
            };
            self.cursor = Some((start, idx + 1));
            if idx >= slot_len {
                continue;
            }
            let buf = self.slots.entry(start).or_insert_with(|| SlotBuf { audio: vec![0.0; slot_len], filled: 0 });
            buf.audio[idx] = y;
            buf.filled += 1;
        }
        // Keep only what the next outputs' filters reach back to.
        let keep_from = (self.next_pos.floor() as u64).saturating_sub(half as u64 + 1);
        if keep_from > self.hist_start {
            let drop = ((keep_from - self.hist_start) as usize).min(self.hist.len());
            self.hist.drain(..drop);
            self.hist_start += drop as u64;
        }
        if latest.is_finite() {
            self.finish_before(latest, slot_ms, slot_len);
        }
    }

    fn filtered(&self, i: u64) -> f32 {
        let half = TAPS / 2;
        let base = i as i64 - half as i64 - self.hist_start as i64;
        // Nearly always the whole filter lies inside the history: a plain
        // dot product the compiler can vectorize.
        if base >= 0
            && let Some(window) = self.hist.get(base as usize..base as usize + TAPS)
        {
            let mut acc = [0f32; 8];
            let (taps, tail) = self.taps.as_chunks::<8>();
            let (xs, xtail) = window.as_chunks::<8>();
            for (h, x) in taps.iter().zip(xs) {
                for l in 0..8 {
                    acc[l] += h[l] * x[l];
                }
            }
            return acc.iter().sum::<f32>() + tail.iter().zip(xtail).map(|(h, x)| h * x).sum::<f32>();
        }
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

    /// Cut every slot that ended before `now_ms` and hand it to a worker.
    fn finish_before(&mut self, now_ms: f64, slot_ms: u64, slot_len: usize) {
        let done: Vec<u64> = self.slots.keys().copied().filter(|&s| (s + slot_ms) as f64 <= now_ms).collect();
        for start in done {
            let Some(buf) = self.slots.remove(&start) else { continue };
            if (buf.filled as f64) < MIN_FILL * slot_len as f64 {
                continue;
            }
            if let Some(dir) = &self.dump {
                let bytes: Vec<u8> = buf.audio.iter().flat_map(|v| v.to_le_bytes()).collect();
                let _ = std::fs::write(dir.join(format!("{start}.f32")), bytes);
            }
            // One slot of a channel at a time: a slot decoded late is worth
            // less than the next one decoded on time.
            if self.busy.swap(true, Ordering::AcqRel) {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let pool = pool();
            let job = Job { protocol: self.protocol, start, audio: buf.audio, band: self.band, cut: Instant::now(), busy: self.busy.clone(), tx: self.tx.clone() };
            pool.waiting.fetch_add(1, Ordering::AcqRel);
            if pool.workers == 0 || pool.jobs.lock().unwrap_or_else(|e| e.into_inner()).send(job).is_err() {
                pool.waiting.fetch_sub(1, Ordering::AcqRel);
                self.busy.store(false, Ordering::Release);
            }
        }
    }

    /// Slots decoded since the last call: (slot start, UNIX ms; decodes).
    pub fn take(&mut self) -> Vec<(u64, Vec<Decode>)> {
        self.rx.try_iter().collect()
    }
}
