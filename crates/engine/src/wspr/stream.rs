//! The streaming front end: sideband audio in at whatever rate the channel
//! runs, even-minute slots out, each decoded off the caller's thread.
//!
//! Audio is mixed down from 1500 Hz and filtered to a 375 Hz complex
//! baseband as it arrives, so a slot in waiting is 45 000 samples rather
//! than five and a half million. Blocks come stamped with the wall-clock
//! time of their first sample. Every baseband sample is given a time from a
//! clock kept by counting input samples; the stamps, which jitter with
//! delivery, are averaged into how far that clock has drifted, and the
//! drift is taken up only where a new slot begins (as the FT8 decoder does,
//! for the same reason: a clock steered within a slot jumps the phase of
//! every tone). The clock is reset outright only when a stamp disagrees by
//! more than a gap would explain. A slot is cut once audio 115 s into it
//! has arrived, which is past the end of any transmission that started on
//! time or up to a few seconds late.

use super::{BASEBAND_HZ, CENTER_HZ, CallHashes, SLOT_KEEP_S, SLOT_MS, WsprDecode, decode_baseband};
use num_complex::Complex32;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

/// Half-amplitude point of the anti-alias filter. Signals are searched
/// within 113 Hz of the center; aliases fold from beyond 375 Hz less that.
const CUTOFF_HZ: f64 = 190.0;
/// Transition width the filter is sized for (a Blackman window takes about
/// 5.5 / length of the rate to fall from pass to stop).
const TRANSITION_HZ: f64 = 90.0;
/// A stamp further than this from the sample clock means audio went
/// missing (or the clock stepped), not jitter.
const RESYNC_MS: f64 = 250.0;
/// A slot with less than this share of its audio is not worth decoding.
const MIN_FILL: f64 = 0.6;

/// Real audio at any rate to the decoder's complex baseband: mixed down
/// from [`CENTER_HZ`], low-pass filtered, and taken every `fs / 375`
/// samples (a fractional step; the nearest input sample is used, a timing
/// error of at most half an input sample, nothing at these tone rates).
pub struct Downconverter {
    fs: f64,
    taps: Vec<f32>,
    /// Mixed input; `hist[0]` is input sample `hist_start - half`, the first
    /// `half` of them zeros so the first output has a full filter.
    hist: Vec<Complex32>,
    hist_start: i64,
    in_count: u64,
    /// Input position of the next output.
    next_pos: f64,
    step: f64,
}

impl Downconverter {
    pub fn new(fs: f32) -> Self {
        let fs = fs as f64;
        let mut len = (5.5 * fs / TRANSITION_HZ).ceil() as usize | 1;
        len = len.max(65);
        let fc = CUTOFF_HZ / fs;
        let mid = (len / 2) as f64;
        let mut taps: Vec<f32> = (0..len)
            .map(|i| {
                let x = i as f64 - mid;
                let sinc = if x == 0.0 { 2.0 * fc } else { (std::f64::consts::TAU * fc * x).sin() / (std::f64::consts::PI * x) };
                let a = std::f64::consts::TAU * i as f64 / (len - 1) as f64;
                let w = 0.42 - 0.5 * a.cos() + 0.08 * (2.0 * a).cos();
                (sinc * w) as f32
            })
            .collect();
        let sum: f32 = taps.iter().sum();
        taps.iter_mut().for_each(|t| *t /= sum);
        let half = len / 2;
        Downconverter { fs, taps, hist: vec![Complex32::new(0.0, 0.0); half], hist_start: 0, in_count: 0, next_pos: 0.0, step: fs / BASEBAND_HZ as f64 }
    }

    fn half(&self) -> usize {
        self.taps.len() / 2
    }

    /// Input samples taken so far.
    pub fn consumed(&self) -> u64 {
        self.in_count
    }

    /// Mix and filter `audio`; every baseband sample now computable is
    /// appended to `out` with the input position (fractional sample index)
    /// it stands for.
    pub fn push(&mut self, audio: &[f32], out: &mut Vec<(f64, Complex32)>) {
        let per = CENTER_HZ as f64 / self.fs;
        for &x in audio {
            // The phase from the sample count, not a running phasor: exact
            // however long the stream runs.
            let ph = -std::f64::consts::TAU * (self.in_count as f64 * per).fract();
            self.hist.push(Complex32::new((x as f64 * ph.cos()) as f32, (x as f64 * ph.sin()) as f32));
            self.in_count += 1;
        }
        self.drain(out, false);
    }

    /// Run the filter out past the last input, as if silence followed.
    pub fn flush(&mut self, out: &mut Vec<(f64, Complex32)>) {
        self.drain(out, true);
    }

    fn drain(&mut self, out: &mut Vec<(f64, Complex32)>, flush: bool) {
        let half = self.half() as i64;
        let len = self.taps.len();
        loop {
            let i = self.next_pos.round() as i64;
            if flush {
                if i >= self.in_count as i64 {
                    break;
                }
            } else if i + half >= self.in_count as i64 {
                break;
            }
            // hist index of input sample i - half.
            let from = (i - self.hist_start) as usize;
            let mut acc = Complex32::new(0.0, 0.0);
            let avail = self.hist.len().saturating_sub(from).min(len);
            for (h, x) in self.taps[..avail].iter().zip(&self.hist[from..from + avail]) {
                acc += x * *h;
            }
            out.push((self.next_pos, acc));
            self.next_pos += self.step;
        }
        // Keep what the next output's filter reaches back to.
        let keep_from = self.next_pos.round() as i64;
        if keep_from > self.hist_start {
            let drop = ((keep_from - self.hist_start) as usize).min(self.hist.len());
            self.hist.drain(..drop);
            self.hist_start += drop as i64;
        }
    }
}

struct SlotBuf {
    z: Vec<Complex32>,
    filled: usize,
}

/// Where the decoder is in the slot cycle.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WsprStatus {
    /// Seconds into the current 2-minute slot by the audio's clock; `None`
    /// before any audio.
    pub slot_secs: Option<f32>,
    /// Audio for the current slot is being kept (false past 115 s, until
    /// the next slot begins).
    pub collecting: bool,
    /// A finished slot is being decoded.
    pub decoding: bool,
    /// Decodes from the last slot decoded, and that slot's start (UNIX ms).
    pub last_count: Option<usize>,
    pub last_slot_ms: Option<u64>,
}

/// WSPR-2 from a stream of audio: push audio as it arrives, take decodes
/// as each slot finishes.
pub struct WsprDecoder {
    input_rate: f64,
    dc: Downconverter,
    /// Input sample index and the UNIX ms it was heard at.
    anchor: Option<(u64, f64)>,
    /// How far the stamps run ahead of the sample clock, ms, averaged; taken
    /// into the anchor when a slot begins.
    drift_ms: f64,
    /// The slot the last sample went to.
    current: Option<u64>,
    /// The next sample's place: (slot start, index in it), counted rather
    /// than recomputed so rounding the clock cannot leave a duplicate and a
    /// hole.
    cursor: Option<(u64, usize)>,
    slots: BTreeMap<u64, SlotBuf>,
    /// The latest slot cut; audio for it or before is not kept again.
    last_cut: Option<u64>,
    latest_ms: Option<f64>,
    hashes: Arc<Mutex<CallHashes>>,
    busy: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    /// Decodes in the last slot decoded (-1 before any) and its start.
    last_count: Arc<AtomicI64>,
    last_slot: Arc<AtomicI64>,
    resyncs: u64,
    out: Vec<(f64, Complex32)>,
    tx: Sender<(u64, Vec<WsprDecode>)>,
    rx: Receiver<(u64, Vec<WsprDecode>)>,
}

impl WsprDecoder {
    pub fn new(fs: f32) -> Self {
        let (tx, rx) = channel();
        WsprDecoder {
            input_rate: fs as f64,
            dc: Downconverter::new(fs),
            anchor: None,
            drift_ms: 0.0,
            current: None,
            cursor: None,
            slots: BTreeMap::new(),
            last_cut: None,
            latest_ms: None,
            hashes: Arc::new(Mutex::new(CallHashes::default())),
            busy: Arc::new(AtomicBool::new(false)),
            dropped: Arc::new(AtomicU64::new(0)),
            last_count: Arc::new(AtomicI64::new(-1)),
            last_slot: Arc::new(AtomicI64::new(-1)),
            resyncs: 0,
            out: Vec::new(),
            tx,
            rx,
        }
    }

    /// Slots skipped because the previous one was still decoding.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Times a stamp disagreed with the sample clock by more than a
    /// delivery's jitter, and the clock was set to it.
    pub fn resyncs(&self) -> u64 {
        self.resyncs
    }

    /// Calls heard in full that type 3 hashes can name.
    pub fn known_calls(&self) -> usize {
        self.hashes.lock().map(|h| h.len()).unwrap_or(0)
    }

    /// Forget the audio in hand (after a retune or a gap). Calls heard so
    /// far are kept.
    pub fn reset(&mut self) {
        self.dc = Downconverter::new(self.input_rate as f32);
        self.anchor = None;
        self.drift_ms = 0.0;
        self.current = None;
        self.cursor = None;
        self.slots.clear();
        self.latest_ms = None;
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
        match self.anchor {
            Some((i, t)) => {
                let err = stamp - (t + (first - i) as f64 * ms_per);
                if err.abs() > RESYNC_MS {
                    self.resyncs += 1;
                    self.anchor = Some((first, stamp));
                    self.drift_ms = 0.0;
                } else {
                    self.drift_ms += (err - self.drift_ms) * 0.02;
                }
            }
            None => self.anchor = Some((first, stamp)),
        }
        let mut out = std::mem::take(&mut self.out);
        out.clear();
        self.dc.push(audio, &mut out);
        let slot_len = (SLOT_MS as f64 / 1000.0 * BASEBAND_HZ as f64) as usize;
        let keep = (SLOT_KEEP_S * BASEBAND_HZ) as usize;
        for &(pos, v) in &out {
            let (ai, at) = self.anchor.expect("set above");
            let mut t = at + (pos - ai as f64) * ms_per;
            if t < 0.0 {
                continue;
            }
            // A new slot: the one place the clock takes up its drift.
            if self.current != Some((t as u64 / SLOT_MS) * SLOT_MS) {
                self.anchor = Some((ai, at + self.drift_ms));
                t += self.drift_ms;
                self.drift_ms = 0.0;
                self.current = Some((t.max(0.0) as u64 / SLOT_MS) * SLOT_MS);
            }
            self.latest_ms = Some(t);
            let start = (t as u64 / SLOT_MS) * SLOT_MS;
            let exact = (t - start as f64) * BASEBAND_HZ as f64 / 1000.0;
            let idx = match self.cursor {
                Some((s, i)) if s == start && (i as f64 - exact).abs() < 0.75 => i,
                _ => exact.round() as usize,
            };
            self.cursor = Some((start, idx + 1));
            if idx >= keep.min(slot_len) || self.last_cut.is_some_and(|c| start <= c) {
                continue;
            }
            let buf = self.slots.entry(start).or_insert_with(|| SlotBuf { z: vec![Complex32::new(0.0, 0.0); keep], filled: 0 });
            buf.z[idx] = v;
            buf.filled += 1;
        }
        self.out = out;
        if let Some(now) = self.latest_ms {
            self.finish_before(now);
        }
    }

    /// Cut every slot whose kept audio has all arrived and hand it to a
    /// worker.
    fn finish_before(&mut self, now_ms: f64) {
        let keep_ms = (SLOT_KEEP_S * 1000.0) as f64;
        let keep = (SLOT_KEEP_S * BASEBAND_HZ) as usize;
        let done: Vec<u64> = self.slots.keys().copied().filter(|&s| s as f64 + keep_ms <= now_ms).collect();
        for start in done {
            let Some(buf) = self.slots.remove(&start) else { continue };
            self.last_cut = Some(self.last_cut.map_or(start, |c| c.max(start)));
            if (buf.filled as f64) < MIN_FILL * keep as f64 {
                continue;
            }
            // Never queue: a slot decoded late is worth less than the next
            // one decoded on time.
            if self.busy.swap(true, Ordering::AcqRel) {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let (hashes, busy, tx, count, slot) = (self.hashes.clone(), self.busy.clone(), self.tx.clone(), self.last_count.clone(), self.last_slot.clone());
            let spawned = std::thread::Builder::new().name("wspr-decode".into()).spawn(move || {
                let decodes = {
                    let mut h = hashes.lock().unwrap_or_else(|e| e.into_inner());
                    decode_baseband(&buf.z, &mut h)
                };
                count.store(decodes.len() as i64, Ordering::Relaxed);
                slot.store(start as i64, Ordering::Relaxed);
                let _ = tx.send((start, decodes));
                busy.store(false, Ordering::Release);
            });
            if spawned.is_err() {
                self.busy.store(false, Ordering::Release);
            }
        }
    }

    /// Decodes from finished 2-minute slots: (slot start, UNIX ms; decodes).
    pub fn take(&mut self) -> Vec<(u64, Vec<WsprDecode>)> {
        self.rx.try_iter().collect()
    }

    pub fn status(&self) -> WsprStatus {
        let slot_secs = self.latest_ms.map(|t| ((t as u64 % SLOT_MS) as f64 / 1000.0) as f32);
        let count = self.last_count.load(Ordering::Relaxed);
        let slot = self.last_slot.load(Ordering::Relaxed);
        WsprStatus {
            slot_secs,
            collecting: slot_secs.is_some_and(|s| s < SLOT_KEEP_S),
            decoding: self.busy.load(Ordering::Relaxed),
            last_count: (count >= 0).then_some(count as usize),
            last_slot_ms: (slot >= 0).then_some(slot as u64),
        }
    }
}
