//! The voice scanner: a peak-driven sweep across a configured frequency range.
//!
//! A classic scanner steps the channel raster and dwells on every channel,
//! which on an RTL-SDR means one slow hardware retune per channel: the tuner
//! faults above one control write per ~250 ms (see `CTL_MIN_INTERVAL`), so a
//! 40 MHz range at 6.25 kHz steps is half an hour per sweep. This scanner
//! instead spends its hardware retunes on *span* moves and finds carriers the
//! cheap way — the wideband FFT already computed for the display covers up to
//! 3.2 MHz at once, and the narrowband inspect chain can re-mix onto any
//! channel inside the current span without touching the tuner at all. The
//! tuner only moves when the current window's candidates are exhausted.
//!
//! Candidates are not tested one at a time: the pool of parallel slots
//! (`cfg.slots`) each carries its own receivers, so a birdie's test dwell no
//! longer hides the short call two channels away. Every sweep frame the idle
//! slots are filled with the strongest untested carriers in the window, and
//! a slot that proves a call holds it — recording its own audio — while the
//! rest of the pool keeps sweeping around it. Several calls can be held at
//! once: the newest takes the speaker, the others record silently, and the
//! window only advances once the last of them has dropped.
//!
//! The state machine is deliberately dumb about *what* a signal is — that
//! judgement belongs to the receivers. It decides *where to listen next* and
//! *whether anything promised a voice call*:
//!
//! - P25/DMR: the channel receivers' frame sync is the whole argument. A
//!   receiver that locked and went in-call is a hit; one that locked and only
//!   ever produced control/grant traffic is a trunked control channel, which
//!   is parked out for a long time (it decodes beautifully and carries no
//!   voice); one that never locked gets a short memory so the sweep moves on
//!   but comes back.
//! - NFM/AM: a hysteresis squelch on the channel SNR says a carrier is
//!   present; energy through the gated voice audio says someone is talking.
//!   FM is near-constant-envelope, so the AM detector hears almost nothing
//!   from it and the discriminator hears almost nothing from AM — whichever
//!   analog path carries voice is what the channel was.
//!
//! Everything is clock-injected (`f64` seconds from the caller) so the whole
//! machine is testable without hardware or real time.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::sdr::{carrier_offset_hz, find_peaks, DecodeHistory};

/// Seconds to stand still after a span move before trusting the spectrum: the
/// per-bin noise floor is seeded high on the first frame and needs a few dozen
/// frames to fall onto the real floor, and judging candidates against a
/// floor that is still falling both misses them and poisons the skip memory.
const SETTLE_S: f64 = 1.5;

/// How long a candidate is test-decoded before a verdict. P25 and DMR need a
/// few hundred milliseconds of good signal to find frame sync; analog needs
/// the AM AGC (~50 ms) and a syllable. Long enough to be sure, short enough
/// that a dense band still sweeps at a useful pace — and with parallel slots
/// the dwell of one candidate no longer serialises behind another's.
const TEST_DWELL_S: f64 = 1.2;

/// A candidate that locked onto a control channel skips this early rather
/// than sitting out the full dwell: the evidence (sync + grants, no voice)
/// is already conclusive.
const CONTROL_VERDICT_S: f64 = 0.5;

/// How long Hunt tolerates an apparently empty window before asking the
/// tuner to move on. A signal that arrives during this window is still seen.
const SWEEP_IDLE_S: f64 = 0.8;

/// Minimum test dwell before an analog lock is believed: lets the AM AGC and
/// the noise gate settle so a noise blip is not mistaken for a first syllable.
const ANALOG_MIN_S: f64 = 0.2;

/// RMS of the gated, levelled voice audio above which someone is talking.
/// The monitor chain attenuates closed-gate noise to around 1e-3 and the
/// leveler brings speech to tenths; the threshold sits in the gap. The analog
/// lock additionally requires the squelch, so this is a second gate, not the
/// only one.
const VOICE_RMS: f32 = 0.02;

/// A candidate that failed to decode — or a channel just released from a
/// lock — is left alone about this long: bands move, so a marginal carrier
/// missed on one pass gets caught on the next, but the sweep moves on to
/// fresh traffic instead of re-locking the call it just left. A released
/// channel gets a much shorter memory than a failed one: the listener who
/// just heard traffic there wants the next call, not a long exile.
const FAILED_RETEST_S: f64 = 60.0;
const RELEASED_REVISIT_S: f64 = 5.0;
/// A trunked control channel is parked out for half an hour: it will still
/// be there, still decoding, still carrying no voice.
const CONTROL_SKIP_S: f64 = 30.0 * 60.0;

fn default_retest_s() -> u32 {
    FAILED_RETEST_S as u32
}

fn default_control_skip_s() -> u32 {
    CONTROL_SKIP_S as u32
}

/// Fails before a carrier is judged persistent noise.
const PERSISTENT_AFTER: u32 = 3;
/// How long persistent noise stays parked: a day. Nothing expires a birdie;
/// only the panel's re-test does.
const PERSISTENT_SKIP_S: u32 = 86_400;

fn default_persistent_after() -> u32 {
    PERSISTENT_AFTER
}

fn default_persistent_skip_s() -> u32 {
    PERSISTENT_SKIP_S
}

/// How many candidates are test-decoded at once. One reproduces the classic
/// serial sweep; more turn the window's birdies into a parallel chore that
/// no longer hides the short call beside them.
const DEFAULT_SLOTS: usize = 4;

/// How far the span advances past the last window's coverage, as a fraction
/// of the span. The overlap keeps a carrier sitting near an edge from falling
/// in the guard band between two windows.
const WINDOW_OVERLAP: f64 = 0.9;

/// Hard frequency bounds, the same clamp the tuning API applies.
const FREQ_MIN_HZ: f64 = 1e6;
const FREQ_MAX_HZ: f64 = 2.2e9;

/// Default squelch hang, bridging the pauses inside one transmission.
const HANG_S: f32 = 1.5;

/// A voice mode the scanner can be asked to look for. A subset of the
/// demodulator modes, because WFM/PACKET/PAGER/AUTO have no place in a voice
/// scan: broadcast FM is one 100 kHz-spaced carrier per endpoint, and the
/// packet/pager families are data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScanMode {
    Am,
    Nfm,
    P25,
    Dmr,
}

impl ScanMode {
    /// The label Last Heard and the decode log use for this mode.
    pub fn protocol(self) -> &'static str {
        match self {
            ScanMode::Am => "AM",
            ScanMode::Nfm => "NFM",
            ScanMode::P25 => "P25",
            ScanMode::Dmr => "DMR",
        }
    }
}

fn default_modes() -> Vec<ScanMode> {
    vec![ScanMode::Nfm, ScanMode::P25, ScanMode::Dmr]
}

fn default_threshold_db() -> f32 {
    scannerd_engine::squelch::DEFAULT_OPEN_DB
}

fn default_resume_s() -> f32 {
    2.0
}

fn default_start() -> f64 {
    144_000_000.0
}

fn default_stop() -> f64 {
    148_000_000.0
}

fn default_slots() -> usize {
    DEFAULT_SLOTS
}

/// What to scan, as configured and persisted. Lives in `usdr.toml` under
/// `[scan]` and comes back to the browser in every status event, so the form
/// always shows what the server is actually doing rather than what it was
/// last sent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScanCfg {
    #[serde(default = "default_start")]
    pub start_hz: f64,
    #[serde(default = "default_stop")]
    pub stop_hz: f64,
    #[serde(default = "default_modes")]
    pub modes: Vec<ScanMode>,
    /// Channel SNR a carrier must reach before the analog squelch will call
    /// it a signal. Digital locking is left to the receivers; this gates the
    /// analog paths, which have no sync to prove them.
    #[serde(default = "default_threshold_db")]
    pub threshold_db: f32,
    /// Seconds after a call drops before the sweep resumes.
    #[serde(default = "default_resume_s")]
    pub resume_delay_s: f32,
    /// How long a carrier that decoded nothing stays skipped.
    #[serde(default = "default_retest_s")]
    pub retest_s: u32,
    /// How long a control channel stays parked out.
    #[serde(default = "default_control_skip_s")]
    pub control_skip_s: u32,
    /// Failed tests after which a carrier is judged persistent noise — a
    /// birdie, a leak, hash — and parked for `persistent_skip_s`. Zero
    /// disables the escalation.
    #[serde(default = "default_persistent_after")]
    pub persistent_after: u32,
    /// How long persistent noise stays parked.
    #[serde(default = "default_persistent_skip_s")]
    pub persistent_skip_s: u32,
    /// How many candidates are test-decoded in parallel. Each slot carries
    /// its own receivers, so the dwells of quiet carriers overlap instead of
    /// serialising in front of a short call.
    #[serde(default = "default_slots")]
    pub slots: usize,
}

impl Default for ScanCfg {
    fn default() -> Self {
        Self {
            start_hz: default_start(),
            stop_hz: default_stop(),
            modes: default_modes(),
            threshold_db: default_threshold_db(),
            resume_delay_s: default_resume_s(),
            retest_s: default_retest_s(),
            control_skip_s: default_control_skip_s(),
            persistent_after: default_persistent_after(),
            persistent_skip_s: default_persistent_skip_s(),
            slots: default_slots(),
        }
    }
}

impl ScanCfg {
    /// Order the endpoints, clamp them to the tunable range, and require at
    /// least one mode. Configuration arrives from a form; it is normalised
    /// rather than rejected, because an inverted range is a typo, not a crime.
    pub fn normalized(mut self) -> Self {
        if self.start_hz > self.stop_hz {
            std::mem::swap(&mut self.start_hz, &mut self.stop_hz);
        }
        self.start_hz = self.start_hz.clamp(FREQ_MIN_HZ, FREQ_MAX_HZ);
        self.stop_hz = self.stop_hz.clamp(FREQ_MIN_HZ, FREQ_MAX_HZ);
        self.modes
            .retain(|m| matches!(m, ScanMode::Am | ScanMode::Nfm | ScanMode::P25 | ScanMode::Dmr));
        if self.modes.is_empty() {
            self.modes = default_modes();
        }
        self.threshold_db = self.threshold_db.clamp(3.0, 25.0);
        self.resume_delay_s = self.resume_delay_s.clamp(0.5, 30.0);
        self.retest_s = self.retest_s.clamp(5, 600);
        self.control_skip_s = self.control_skip_s.clamp(30, 14400);
        self.persistent_after = self.persistent_after.clamp(0, 10);
        self.persistent_skip_s = self.persistent_skip_s.clamp(300, 604800);
        self.slots = self.slots.clamp(1, 8);
        self
    }
}

/// What the scanner is doing, for the status event and the panel.
#[derive(Clone, Debug, Serialize, Default)]
pub struct ScanStatus {
    /// "settling" | "sweeping" | "testing" | "locked" | "paused"
    pub state: String,
    /// The frequency under test, locked, or the span centre while settling.
    pub freq_hz: f64,
    /// Where the sweep stands, e.g. "span 2/9 · slots 3/4 · 7 candidates".
    pub progress: String,
    /// Candidates in the current window.
    pub candidates: usize,
    /// Frequencies under parallel test right now, for the panel markers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub testing: Vec<f64>,
    /// Frequencies of the calls being held (and recorded) right now. The
    /// newest of them is on the speaker; the rest record silently.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locked: Vec<f64>,
    /// Voice hits this session.
    pub hits: usize,
    /// Learned skips remembered.
    pub skipped: usize,
    /// The effective configuration.
    pub cfg: ScanCfg,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The learned skip list, for the panel. Empty when nothing is skipped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skip_list: Vec<SkipEntry>,
}

/// One step the scanner wants the SDR worker to take. The worker executes
/// these against the real chain; the scanner never touches hardware state.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Point a parallel slot at this frequency, building its receivers if it
    /// has none. Free: it happens inside the current span, no tuner write.
    Inspect { slot: usize, freq_hz: f64 },
    /// Tear a slot's receivers down: the slot has no candidate to listen to.
    Release { slot: usize },
    /// Hardware-retune the span centre to this frequency. Rate-limited and
    /// confirmed by the worker like any other tune.
    TuneSpan(f64),
    /// A one-line explanation for the decode log ("control channel — parked
    /// out"), so a skipped frequency is accounted for, not just silent.
    Notice(String),
}

/// Operator intent, from the control endpoint: pause/resume the sweep, skip
/// whatever is under test or locked, forget the learned skip memory, or
/// re-test one specific frequency.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScanControl {
    Pause,
    Resume,
    Skip,
    Forget,
    Unskip,
}

/// Frame sync and call state from one of the digital voice receivers, tagged
/// with which one produced it — when both run, both can lock, and the label
/// belongs to whichever receiver is actually carrying the call.
#[derive(Clone, Copy, Debug)]
pub struct DigitalEvidence {
    pub mode: ScanMode,
    /// Frame sync acquired.
    pub locked: bool,
    /// A voice call is in progress.
    pub in_call: bool,
    /// Control-plane activity this block (a trunking grant, a data frame):
    /// the signature of a control channel, which decodes well and carries no
    /// voice.
    pub control: bool,
}

/// Per-block evidence from one slot's candidate, while it is under test or
/// locked. The worker produces one of these per running slot per IQ block.
#[derive(Clone, Copy, Debug, Default)]
pub struct Signal {
    /// Channel SNR at the inspected frequency, in dB, from the spectrum.
    pub snr_db: f32,
    pub digital: Option<DigitalEvidence>,
    /// RMS of the gated NFM monitor audio, when NFM is being run.
    pub nfm_voice_rms: Option<f32>,
    /// RMS of the AM envelope audio, when AM is being run.
    pub am_voice_rms: Option<f32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Skip {
    /// Decoded nothing worth staying for; look again soon.
    Failed,
    /// Trunked control channel: strong, decodable, and not voice.
    Control,
    /// A call was just held here; give the sweep a head start but come back
    /// quickly — the listener wants the next call, not a long exile.
    Released,
    /// Failed to decode often enough to be a birdie or some other persistent
    /// non-voice carrier: parked for a very long stretch. Reversible only
    /// from the panel, which is exactly what makes an aggressive park safe.
    Persistent,
}

impl Skip {
    /// The panel-facing reason for the skip.
    fn label(self) -> &'static str {
        match self {
            Skip::Failed => "no decode",
            Skip::Control => "control channel",
            Skip::Released => "recently held",
            Skip::Persistent => "persistent noise",
        }
    }
}

/// One learned skip, for the panel: where, why, and how long is left.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkipEntry {
    pub freq_hz: f64,
    pub reason: String,
    pub expires_in_s: u32,
    /// How many times this carrier has failed a test — the escalation
    /// counter behind a "persistent noise" park.
    pub fails: u32,
}

/// Per-block recording state of one slot's call, for the worker. A slot
/// holding a call records its own audio — digital on the receiver's call
/// events, analog while this squelch is open — whether or not it is the
/// call on the speaker.
#[derive(Clone, Copy, Debug)]
pub struct SlotRecording {
    pub mode: ScanMode,
    /// Whether the analog squelch currently has the channel. Only meaningful
    /// for an analog `mode`; a digital call is bracketed by its call events.
    pub analog_open: bool,
    /// Seconds the analog squelch has been closed, for trimming the tail off
    /// a recording that outlived the talking.
    pub trailing_s: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    /// After a span move: let the noise floor fall onto the real floor.
    /// `None` stamps itself on the first frame that observes it, because the
    /// states entered from configuration changes have no clock yet.
    Settle { since: Option<f64> },
    /// The working state — the only one. Held calls live in the slots that
    /// earned them; the machine keeps hunting around them until the window
    /// is exhausted and the last call has dropped.
    Hunt,
}

struct Candidate {
    freq_hz: f64,
    snr_db: f32,
}

/// One parallel test channel: the candidate it is judging — or the call it
/// is holding — and the evidence gathered so far. The receivers themselves
/// live in the worker; this is the state machine's half of the slot.
struct Slot {
    freq_hz: f64,
    /// When the slot's current phase — test dwell or held call — began.
    since_s: f64,
    /// The call this slot is holding. A held slot keeps decoding (and
    /// recording) its call while the rest of the pool keeps sweeping; the
    /// window cannot advance while any slot is held, because a slot
    /// receiver only sees its channel inside the current span.
    locked: Option<ScanMode>,
    /// Monotonic sequence number of the lock, so "the newest call takes the
    /// speaker" survives lookups after older calls end.
    lock_seq: u64,
    locked_seen: bool,
    control_seen: bool,
    nfm_voice_seen: bool,
    am_voice_seen: bool,
    squelch: scannerd_engine::Squelch,
    /// Whether the analog squelch currently has the channel: the per-slot
    /// recording gate for an analog call.
    analog_open: bool,
    /// Seconds the held call has been quiet, for the resume delay.
    quiet_since_s: Option<f64>,
}

impl Slot {
    fn new(freq_hz: f64, now: f64, cfg: &ScanCfg) -> Self {
        Self {
            freq_hz,
            since_s: now,
            locked: None,
            lock_seq: 0,
            locked_seen: false,
            control_seen: false,
            nfm_voice_seen: false,
            am_voice_seen: false,
            // Hysteresis 3 dB under the open threshold, so a carrier sitting
            // on the edge holds a lock without having been able to start one.
            squelch: scannerd_engine::Squelch::new(
                cfg.threshold_db,
                cfg.threshold_db - 3.0,
                HANG_S,
            ),
            analog_open: false,
            quiet_since_s: None,
        }
    }
}

/// What one block of evidence earned a slot. `None` means the dwell goes on.
enum Verdict {
    Keep,
    Lock(ScanMode),
    Control,
    /// The candidate is done; the payload, when present, explains why in the
    /// decode log.
    Fail(Option<String>),
}

/// The scanner state machine. Driven from two places in the SDR worker: one
/// [`observe_frame`](Self::observe_frame) per display frame (25/s, where the
/// spectrum is fresh) and one [`note_signals`](Self::note_signals) per IQ
/// block (where the receivers report).
pub struct Scanner {
    cfg: ScanCfg,
    paused: bool,
    state: State,

    // Geometry of the window being watched. `tuned_hz` is the CONFIRMED span
    // centre: acting on a requested tune that the dongle refused would point
    // every candidate at empty air.
    tuned_hz: f64,
    span_rate: f64,

    // Candidate measurement. The floor is seeded high on the first frame of a
    // window and falls onto the real noise floor during Settle — a fresh
    // min-statistics tracker seeded on the spectrum itself would adopt steady
    // carriers as their own floor and never see them again.
    floor: scannerd_dsp::NoiseFloor,
    floor_len: usize,
    /// The floor snapshot handed to the state step each frame: taken out,
    /// refilled, and restored so its capacity survives (see `observe_frame`).
    floor_buf: Vec<f32>,
    scratch: Vec<f32>,
    history: DecodeHistory,

    // Learned skips, keyed by frequency in kHz (candidate frequencies carry
    // sub-kHz centroid detail the memory does not need). The counter is how
    // many times the carrier has failed a test — the escalation toward
    // "persistent noise".
    skips: HashMap<u64, (f64, Skip, u32)>,

    // The sweep.
    cands: Vec<Candidate>,
    span_index: usize,
    /// When the window last looked empty and idle: the clock behind the
    /// linger before a span move. Refreshed whenever anything is being
    /// tested or anything testable is on the spectrum.
    hunt_since_s: f64,
    /// A window the sweep must move to before it can continue — set when a
    /// reconfiguration lands the radio somewhere the new range cannot see.
    pending_window: Option<usize>,

    // The parallel test pool. A `Some` slot is running in the worker; a
    // `None` slot is idle and up for the next candidate. A slot holding a
    // call stays `Some` — and keeps the window pinned — until the call
    // drops and the resume delay has passed.
    slots: Vec<Option<Slot>>,

    // The speaker. The newest call still held holds the audio: the worker
    // routes LISTEN and the shared inspect chain from that slot, while
    // every other held call just records. `lock_seq` stamps each lock so
    // "newest" survives older calls ending first.
    listen_slot: Option<usize>,
    lock_seq: u64,
    hits: usize,

    msg: Option<String>,
    dirty: bool,
}

impl Scanner {
    pub fn new(cfg: ScanCfg) -> Self {
        let cfg = cfg.normalized();
        Self {
            slots: (0..cfg.slots).map(|_| None).collect(),
            cfg,
            paused: false,
            state: State::Settle { since: None },
            tuned_hz: 0.0,
            span_rate: 0.0,
            floor: scannerd_dsp::NoiseFloor::new(),
            floor_len: 0,
            floor_buf: Vec::new(),
            scratch: Vec::new(),
            history: DecodeHistory::new(),
            skips: HashMap::new(),
            cands: Vec::new(),
            hunt_since_s: 0.0,
            span_index: 0,
            pending_window: None,
            listen_slot: None,
            lock_seq: 0,
            hits: 0,
            msg: None,
            dirty: true,
        }
    }

    /// The effective configuration (tests read it through the status event).
    #[cfg(test)]
    pub fn cfg(&self) -> &ScanCfg {
        &self.cfg
    }

    /// Apply a new configuration mid-scan: the sweep restarts, and the tuner
    /// is walked to wherever the new range wants it — a reconfiguration is
    /// not allowed to leave the sweep watching a span that cannot see the
    /// range any more. The learned skips survive, because a control channel
    /// is still one.
    pub fn set_cfg(&mut self, cfg: ScanCfg) -> Vec<Action> {
        self.cfg = cfg.normalized();
        let acts = self.reseat();
        // The pool follows the configured size: a shrink drops the (already
        // freed) spare slots, a grow adds fresh idle ones.
        self.slots.resize_with(self.cfg.slots, || None);
        acts
    }

    /// The span sample rate changed: every window boundary moves.
    pub fn set_span(&mut self, rate_hz: f64) -> Vec<Action> {
        self.span_rate = rate_hz;
        self.floor_len = 0;
        self.cands.clear();
        self.listen_slot = None;
        let mut acts = Vec::new();
        self.release_all_slots(&mut acts);
        self.state = State::Settle { since: None };
        self.dirty = true;
        acts
    }

    /// After configuration or an operator takeover changed where the radio
    /// sits relative to the range, decide which window the sweep calls home:
    /// the window containing the tuning when that is inside the range, and
    /// the range's first window when it is not. Whatever the pool was doing,
    /// it stops — a reseat is also how a resume discards slots parked against
    /// a span that has since moved.
    fn reseat(&mut self) -> Vec<Action> {
        let half = self.span_rate / 2.0;
        let overlaps = self.span_rate > 0.0
            && self.tuned_hz + half > self.cfg.start_hz
            && self.tuned_hz - half < self.cfg.stop_hz;
        if overlaps {
            self.sync_span_index();
            // Whatever move an earlier reseat asked for is moot: the tuning
            // is back inside the range, and honouring a stale pending_window
            // would bounce the tuner to window 0 for no reason.
            self.pending_window = None;
        } else {
            self.span_index = 0;
            self.pending_window = Some(0);
        }
        self.cands.clear();
        self.listen_slot = None;
        let mut acts = Vec::new();
        self.release_all_slots(&mut acts);
        self.state = State::Settle { since: None };
        self.dirty = true;
        acts
    }

    /// Free every slot, telling the worker to drop each one's receivers.
    fn release_all_slots(&mut self, acts: &mut Vec<Action>) {
        for i in 0..self.slots.len() {
            if self.slots[i].take().is_some() {
                acts.push(Action::Release { slot: i });
            }
        }
    }

    /// Free one slot if it was running.
    fn free_slot(&mut self, i: usize, acts: &mut Vec<Action>) {
        if self.slots[i].take().is_some() {
            acts.push(Action::Release { slot: i });
            self.dirty = true;
        }
    }

    /// Park the sweep. `reason` distinguishes the two ways it happens — the
    /// operator taking the dial, or the panel's pause button — because the
    /// status line says which.
    ///
    /// A paused scanner never runs the release logic in `note_signals`, so a
    /// held call would freeze mid-air: speaker pinned to it, window pinned to
    /// it, recorders running, and after a hand-tune the receiver left staring
    /// at a carrier that is no longer in the span. Resume reseats and drops
    /// every slot anyway, so pause releases them too — the calls end here,
    /// file what they have, and the sweep is genuinely stopped.
    pub fn pause(&mut self, reason: &str) -> Vec<Action> {
        let mut acts = Vec::new();
        if !self.paused {
            self.paused = true;
            self.msg = Some(format!("paused — {reason}"));
            self.dirty = true;
            self.listen_slot = None;
            self.release_all_slots(&mut acts);
        }
        acts
    }

    /// Resume from the current span, wherever the operator left the radio.
    pub fn resume(&mut self) -> Vec<Action> {
        if self.paused {
            self.paused = false;
            self.msg = None;
            return self.reseat();
        }
        Vec::new()
    }

    /// Skip whatever is running, right now: every held call is released and
    /// every test dwell abandoned — the operator has heard enough of this
    /// stretch of the band.
    pub fn skip(&mut self, now: f64) -> Vec<Action> {
        let mut acts = Vec::new();
        for i in 0..self.slots.len() {
            let running = self.slots[i].as_ref().map(|s| (s.freq_hz, s.locked));
            if let Some((f, locked)) = running {
                if locked.is_some() {
                    self.remember(f, Skip::Released, now);
                } else {
                    self.note_failure(f, now);
                }
            }
            self.free_slot(i, &mut acts);
        }
        self.listen_slot = None;
        self.hunt_since_s = now;
        self.dirty = true;
        acts
    }

    pub fn forget(&mut self) {
        self.skips.clear();
        self.msg = Some("learned skips forgotten".into());
        self.dirty = true;
    }

    /// Whether any slot is holding a call. The worker's wiring reads
    /// `listen` directly; this is the state-machine-level predicate.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_locked(&self) -> bool {
        self.slots.iter().flatten().any(|s| s.locked.is_some())
    }

    /// The mode of the call on the speaker, if any.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn locked_mode(&self) -> Option<ScanMode> {
        self.listen().map(|(_, mode)| mode)
    }

    /// The slot carrying the speaker — the newest call still held — and its
    /// mode. The worker routes LISTEN audio, the shared inspect chain and
    /// the passband marker from it; every other held call just records.
    pub fn listen(&self) -> Option<(usize, ScanMode)> {
        let i = self.listen_slot?;
        let slot = self.slots.get(i)?.as_ref()?;
        Some((i, slot.locked?))
    }

    /// Frequencies of every call being held right now, for the status event
    /// and the spectrum markers.
    pub fn locked_freqs(&self) -> Vec<f64> {
        self.slots
            .iter()
            .flatten()
            .filter(|s| s.locked.is_some())
            .map(|s| s.freq_hz)
            .collect()
    }

    /// Per-slot recording state for the worker: which slots are in a call,
    /// on which mode, whether the analog squelch is open (the analog
    /// recording gate), and how long that squelch has been closed (for
    /// trimming the tail). A `None` slot is not in a call.
    pub fn recording_states(&self) -> Vec<Option<SlotRecording>> {
        self.slots
            .iter()
            .map(|s| {
                s.as_ref().and_then(|slot| {
                    slot.locked.map(|mode| SlotRecording {
                        mode,
                        analog_open: slot.analog_open,
                        trailing_s: slot.squelch.trailing_silence_s(),
                    })
                })
            })
            .collect()
    }

    /// The held slot with the highest lock sequence — the newest call still
    /// on the air.
    fn newest_locked_slot(&self) -> Option<usize> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                s.as_ref()
                    .filter(|s| s.locked.is_some())
                    .map(|s| (s.lock_seq, i))
            })
            .max_by_key(|(seq, _)| *seq)
            .map(|(_, i)| i)
    }

    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// One display frame: fresh full-span spectrum, in dB, centred on the
    /// confirmed tuning. This is where candidates are found, idle slots are
    /// filled, and windows advance. `usable_half_hz` bounds how far from
    /// centre a candidate may sit and still be inspectable.
    pub fn observe_frame(
        &mut self,
        now: f64,
        tuned_hz: f64,
        span_rate: f64,
        usable_half_hz: f64,
        smoothed: &[f32],
        spur_offsets_hz: &[f64],
    ) -> Vec<Action> {
        if self.paused {
            return Vec::new();
        }
        // Geometry: a new span rate or a moved tuner restarts the window —
        // candidates measured against the old centre point at frequencies
        // the radio is no longer on. (Scan-initiated span hops arrive here
        // too, which is what makes the post-hop settle happen.)
        if self.floor_len != smoothed.len() || span_rate != self.span_rate {
            self.span_rate = span_rate;
            self.floor_len = smoothed.len();
            self.seed_floor(smoothed);
            self.cands.clear();
            self.state = State::Settle { since: None };
        }
        if self.tuned_hz == 0.0 || (tuned_hz - self.tuned_hz).abs() > 1.0 {
            self.tuned_hz = tuned_hz;
            self.seed_floor(smoothed);
            self.cands.clear();
            self.state = State::Settle { since: None };
            // A tuning confirmed outside the range means the sweep is parked
            // where it can see none of it. Mirror `reseat`: home the index on
            // window 0, the bottom of the range. Without this the first
            // advance would step from the fresh scanner's index 0 to window 1
            // and the bottom of the range would go unvisited until the sweep
            // wrapped all the way around.
            let half = self.span_rate / 2.0;
            let overlaps = half > 0.0
                && tuned_hz + half > self.cfg.start_hz
                && tuned_hz - half < self.cfg.stop_hz;
            if !overlaps {
                self.span_index = 0;
                self.pending_window = Some(0);
            }
        }

        // The floor falls every frame, in every state — it is cheapest to
        // keep it current than to decide when it matters. The buffer is
        // taken out and restored (not copied) so the state step below can
        // hold &mut self against it: a fresh Vec here would otherwise be
        // 25 full-FFT-size allocations a second, forever.
        let mut floor = std::mem::take(&mut self.floor_buf);
        floor.clear();
        floor.extend_from_slice(self.floor.update(smoothed));
        let acts = self.step(now, usable_half_hz, smoothed, spur_offsets_hz, &floor);
        self.floor_buf = floor;
        acts
    }

    /// The state machine proper, run against the freshly updated floor.
    fn step(
        &mut self,
        now: f64,
        usable_half_hz: f64,
        smoothed: &[f32],
        spur_offsets_hz: &[f64],
        floor: &[f32],
    ) -> Vec<Action> {
        if matches!(self.state, State::Settle { .. }) {
            let State::Settle { since } = self.state else {
                unreachable!("matched Settle above")
            };
            let since = match since {
                Some(s) => s,
                None => {
                    self.state = State::Settle { since: Some(now) };
                    now
                }
            };
            if now - since >= SETTLE_S {
                // A reconfiguration may have parked the sweep on a window
                // the tuner is not actually on: move before sweeping, or
                // the candidate list would be read off the wrong span.
                // The settle restarts so it runs at the new span, after
                // the confirmed tuning arrives.
                if let Some(k) = self.pending_window.take() {
                    self.span_index = k;
                    let centre = self.window_centre(k);
                    self.state = State::Settle { since: None };
                    return vec![Action::TuneSpan(centre)];
                }
                self.state = State::Hunt;
                self.hunt_since_s = now;
                self.dirty = true;
            }
            return Vec::new();
        }
        // Hunt runs in every working frame, calls and all: held slots sit
        // out of the candidate pick (their frequencies are already taken),
        // and the window is only advanced when the pool is completely idle.
        self.hunt(now, usable_half_hz, smoothed, spur_offsets_hz, floor)
    }

    /// Find candidates in the fresh spectrum, fill the idle slots with the
    /// strongest of them, and — when nothing anywhere asks for attention —
    /// move the window onward.
    fn hunt(
        &mut self,
        now: f64,
        usable_half_hz: f64,
        smoothed: &[f32],
        spur_offsets_hz: &[f64],
        floor: &[f32],
    ) -> Vec<Action> {
        let peaks = find_peaks(
            smoothed,
            self.tuned_hz,
            self.span_rate,
            spur_offsets_hz,
            floor,
            &self.history,
        );

        self.cands.clear();
        let mut assigned: Vec<u64> = self
            .slots
            .iter()
            .flatten()
            .map(|s| khz_key(s.freq_hz))
            .collect();
        for p in peaks {
            // The configured range, the inspectable part of the span, and a
            // cheap pre-filter at the squelch threshold. The decode test is
            // the real gate; this only keeps obviously-empty carriers from
            // spending a dwell on noise the classifier would lecture about.
            if p.freq_hz < self.cfg.start_hz || p.freq_hz > self.cfg.stop_hz {
                continue;
            }
            if (p.freq_hz - self.tuned_hz).abs() > usable_half_hz {
                continue;
            }
            if p.snr_db + 3.0 < self.cfg.threshold_db {
                continue;
            }
            if self.skipped_at(p.freq_hz, now).is_some() {
                continue;
            }
            // Bin resolution is ~2 kHz; the centroid lands the receiver on
            // the carrier itself so the detectors start centred.
            let off = carrier_offset_hz(
                smoothed,
                self.span_rate,
                p.freq_hz - self.tuned_hz,
                16_000.0,
                &mut self.scratch,
            )
            .unwrap_or(0.0);
            self.cands.push(Candidate {
                freq_hz: p.freq_hz + off,
                snr_db: p.snr_db,
            });
        }
        self.cands
            .sort_by(|a, b| a.freq_hz.total_cmp(&b.freq_hz));

        // Fill the idle slots, strongest carrier first. A candidate already
        // being listened to by another slot is not a second candidate, so
        // the pool never doubles up on one carrier (the kHz key absorbs the
        // centroid jitter between frames).
        let mut acts = Vec::new();
        let mut order: Vec<usize> = (0..self.cands.len()).collect();
        order.sort_by(|&a, &b| self.cands[b].snr_db.total_cmp(&self.cands[a].snr_db));
        let mut next = order.into_iter();
        for i in 0..self.slots.len() {
            if self.slots[i].is_some() {
                continue;
            }
            let pick = next.find(|&ci| {
                let key = khz_key(self.cands[ci].freq_hz);
                !assigned.contains(&key)
            });
            let Some(ci) = pick else { break };
            let freq = self.cands[ci].freq_hz;
            assigned.push(khz_key(freq));
            self.slots[i] = Some(Slot::new(freq, now, &self.cfg));
            acts.push(Action::Inspect {
                slot: i,
                freq_hz: freq,
            });
            self.dirty = true;
        }

        // The window is only "empty" for the linger timer when no slot is
        // listening and no candidate is waiting; anything else is progress.
        if self.slots.iter().any(|s| s.is_some()) || !self.cands.is_empty() {
            self.hunt_since_s = now;
        } else if now - self.hunt_since_s >= SWEEP_IDLE_S {
            acts.extend(self.advance_window(now));
        }
        acts
    }

    /// Per-block receiver evidence for every running slot, judged while the
    /// sweep continues. `sigs` is indexed by slot; `None` means the worker
    /// has no receivers on that slot this block. `block_s` is how much time
    /// this block represents.
    ///
    /// A slot under test can prove a call and be promoted; a slot holding a
    /// call is held or released on its own evidence. Both happen without
    /// disturbing the rest of the pool — several calls can be held at once,
    /// and the newest takes the speaker.
    pub fn note_signals(&mut self, now: f64, sigs: &[Option<Signal>], block_s: f32) -> Vec<Action> {
        if self.paused {
            return Vec::new();
        }
        match self.state {
            State::Settle { .. } => Vec::new(),
            State::Hunt => {
                let resume_s = f64::from(self.cfg.resume_delay_s);
                let mut acts = Vec::new();
                // Verdicts are collected while the slots are borrowed and
                // applied afterwards, when the machine is free again.
                let mut promotions: Vec<(usize, ScanMode, f32)> = Vec::new();
                let mut releases: Vec<usize> = Vec::new();
                for i in 0..self.slots.len() {
                    let Some(slot) = self.slots[i].as_mut() else {
                        continue;
                    };
                    let sig = sigs.get(i).copied().flatten().unwrap_or_default();
                    if let Some(mode) = slot.locked {
                        // Holding a call: the same evidence that won it
                        // keeps it, until it has been quiet past the resume
                        // delay. Digital holds on the receiver's call
                        // state; analog on the slot's own squelch.
                        let call_present = match mode {
                            ScanMode::P25 | ScanMode::Dmr => {
                                let in_call = sig.digital.is_some_and(|d| d.in_call);
                                if in_call {
                                    slot.quiet_since_s = None;
                                } else if slot.quiet_since_s.is_none() {
                                    slot.quiet_since_s = Some(now);
                                }
                                in_call
                            }
                            _ => {
                                let open = slot.squelch.update(sig.snr_db, block_s);
                                slot.analog_open = open;
                                if open {
                                    slot.quiet_since_s = None;
                                } else if slot.quiet_since_s.is_none() {
                                    slot.quiet_since_s = Some(now);
                                }
                                open
                            }
                        };
                        if !call_present
                            && slot.quiet_since_s.is_some_and(|q| now - q >= resume_s)
                        {
                            releases.push(i);
                        }
                        continue;
                    }
                    let dwell = now - slot.since_s;
                    match judge_slot(slot, dwell, sig, block_s) {
                        Verdict::Keep => {}
                        Verdict::Lock(mode) => promotions.push((i, mode, sig.snr_db)),
                        Verdict::Control => {
                            let f = self.slots[i].as_ref().expect("slot just judged").freq_hz;
                            self.remember(f, Skip::Control, now);
                            self.free_slot(i, &mut acts);
                            self.hunt_since_s = now;
                            let m = "control channel — parked out".to_string();
                            self.msg = Some(m.clone());
                            acts.push(Action::Notice(m));
                        }
                        Verdict::Fail(notice) => {
                            let f = self.slots[i].as_ref().expect("slot just judged").freq_hz;
                            self.note_failure(f, now);
                            self.free_slot(i, &mut acts);
                            self.hunt_since_s = now;
                            if let Some(m) = notice {
                                self.msg = Some(m.clone());
                                acts.push(Action::Notice(m));
                            }
                        }
                    }
                }
                // Every slot that proved a call holds it: the pool is not
                // torn down, so the sweep keeps testing around the calls
                // and a second carrier proving a minute later records too.
                for (i, mode, _) in &promotions {
                    let Some(slot) = self.slots[*i].as_mut() else {
                        continue;
                    };
                    self.lock_seq += 1;
                    slot.locked = Some(*mode);
                    slot.lock_seq = self.lock_seq;
                    slot.since_s = now;
                    slot.analog_open = !matches!(mode, ScanMode::P25 | ScanMode::Dmr);
                    slot.quiet_since_s = None;
                    self.hits += 1;
                    self.history.note(slot.freq_hz, mode.protocol());
                    self.msg = Some(format!(
                        "locked {} at {:.4} MHz",
                        mode.protocol(),
                        slot.freq_hz / 1e6
                    ));
                    self.dirty = true;
                }
                // The speaker goes to the call the listener heard most
                // recently; when several prove in one block, the strongest
                // carrier wins the argument.
                if let Some(&(i, _, _)) = promotions.iter().max_by(|a, b| a.2.total_cmp(&b.2)) {
                    self.listen_slot = Some(i);
                } else if releases.iter().any(|&i| self.listen_slot == Some(i)) {
                    // The call on the speaker ended: fall back to the
                    // newest call still held, so overlapping traffic is
                    // heard through instead of sandwiching silence.
                    self.listen_slot = self.newest_locked_slot();
                    self.msg = match self.listen() {
                        Some((j, m)) => {
                            let f = self.slots[j].as_ref().map(|s| s.freq_hz).unwrap_or(0.0);
                            Some(format!(
                                "listening to {} at {:.4} MHz",
                                m.protocol(),
                                f / 1e6
                            ))
                        }
                        None => None,
                    };
                }
                for i in releases {
                    self.release_slot(i, now, &mut acts);
                }
                acts
            }
        }
    }

    /// One held call dropped and its resume delay passed: that slot is
    /// freed for the next candidate and the channel gets the short
    /// released memory. Any other held calls carry on untouched.
    fn release_slot(&mut self, i: usize, now: f64, acts: &mut Vec<Action>) {
        let Some(slot) = self.slots[i].as_ref() else {
            return;
        };
        let f = slot.freq_hz;
        let was_listen = self.listen_slot == Some(i);
        self.remember(f, Skip::Released, now);
        self.free_slot(i, acts);
        if was_listen {
            self.listen_slot = self.newest_locked_slot();
            self.msg = match self.listen() {
                Some((j, m)) => {
                    let f = self.slots[j].as_ref().map(|s| s.freq_hz).unwrap_or(0.0);
                    Some(format!(
                        "listening to {} at {:.4} MHz",
                        m.protocol(),
                        f / 1e6
                    ))
                }
                None => None,
            };
        }
        self.hunt_since_s = now;
        self.dirty = true;
    }

    /// Candidates are gone from this window: move the tuner to the next
    /// stretch of the range, wrapping at the top.
    fn advance_window(&mut self, now: f64) -> Vec<Action> {
        // Prune expired entries here, at the slow end of the sweep — after
        // this, the skip count in the status means exactly what is still
        // being skipped, and the panel's list agrees with it.
        self.skips
            .retain(|_, (at, kind, _)| now - *at < Self::ttl_of(*kind, &self.cfg));
        let n = self.window_count();
        let next = (self.span_index + 1) % n;
        self.span_index = next;
        let centre = self.window_centre(next);
        self.cands.clear();
        let mut acts = Vec::new();
        self.release_all_slots(&mut acts);
        self.state = State::Settle { since: None };
        self.dirty = true;
        // A range that fits in one window has nowhere to advance to: the
        // sweep simply starts over on the same spectrum.
        if n > 1 {
            acts.push(Action::TuneSpan(centre));
            if next == 0 {
                self.msg = Some("sweep complete — restarting".into());
                acts.push(Action::Notice(self.msg.clone().expect("just set")));
            }
        }
        acts
    }

    fn window_step(&self) -> f64 {
        self.span_rate * WINDOW_OVERLAP
    }

    /// How many span windows cover the configured range.
    fn window_count(&self) -> usize {
        if self.span_rate <= 0.0 {
            return 1;
        }
        let range = (self.cfg.stop_hz - self.cfg.start_hz).max(0.0);
        if range <= self.span_rate {
            1
        } else {
            ((range - self.span_rate) / self.window_step()).ceil() as usize + 1
        }
    }

    /// Centre of the kth window, clamped so it always covers a stretch of the
    /// range rather than hanging off one end.
    fn window_centre(&self, k: usize) -> f64 {
        let half = self.span_rate / 2.0;
        let lo = self.cfg.start_hz + half;
        let hi = self.cfg.stop_hz - half;
        if hi >= lo {
            (lo + k as f64 * self.window_step()).clamp(lo, hi)
        } else {
            // The range is narrower than one span: a single window, centred
            // on the middle of the range.
            (self.cfg.start_hz + self.cfg.stop_hz) / 2.0
        }
    }

    /// Sync the window index with wherever the radio actually is (used when
    /// resuming after the operator moved the dial).
    pub fn sync_span_index(&mut self) {
        if self.span_rate <= 0.0 {
            return;
        }
        let step = self.window_step();
        if step <= 0.0 {
            self.span_index = 0;
            return;
        }
        let base = self.cfg.start_hz + self.span_rate / 2.0;
        let k = ((self.tuned_hz - base) / step).round();
        self.span_index = (k.max(0.0) as usize).min(self.window_count().saturating_sub(1));
    }

    /// The scan-panel view of the skip memory: where, why, and how long
    /// until the sweep will look again. Newest-relevant order (by frequency).
    pub fn skip_list(&self, now: f64) -> Vec<SkipEntry> {
        let mut entries: Vec<SkipEntry> = self
            .skips
            .iter()
            .map(|(khz_key_mem, (at, kind, fails))| SkipEntry {
                freq_hz: *khz_key_mem as f64 * 1000.0,
                reason: kind.label().into(),
                expires_in_s: (self.ttl(*kind) - (now - at)).max(0.0) as u32,
                fails: *fails,
            })
            .collect();
        entries.sort_by(|a, b| {
            b.fails
                .cmp(&a.fails)
                .then(a.freq_hz.total_cmp(&b.freq_hz))
        });
        entries.truncate(200);
        entries
    }

    /// Re-test one frequency before its skip expires. The memory is keyed
    /// by kHz and candidates carry centroid detail, so a small neighbourhood
    /// is cleared rather than one exact key.
    pub fn unskip(&mut self, freq_hz: f64) {
        let base = khz_key(freq_hz) as i64;
        for d in -2..=2 {
            self.skips.remove(&((base + d).max(0) as u64));
        }
        self.msg = Some(format!("will re-test {:.4} MHz", freq_hz / 1e6));
        self.dirty = true;
    }

    fn ttl(&self, kind: Skip) -> f64 {
        Self::ttl_of(kind, &self.cfg)
    }

    /// Expiry for a skip kind, free of `self` so a prune can consult it
    /// while holding the map mutably.
    fn ttl_of(kind: Skip, cfg: &ScanCfg) -> f64 {
        match kind {
            Skip::Failed => f64::from(cfg.retest_s),
            Skip::Released => RELEASED_REVISIT_S,
            Skip::Control => f64::from(cfg.control_skip_s),
            Skip::Persistent => f64::from(cfg.persistent_skip_s),
        }
    }

    fn skipped_at(&self, freq_hz: f64, now: f64) -> Option<Skip> {
        let key = khz_key(freq_hz);
        match self.skips.get(&key) {
            Some((at, kind, _)) if now - at < self.ttl(*kind) => Some(*kind),
            _ => None,
        }
    }

    fn remember(&mut self, freq_hz: f64, kind: Skip, now: f64) {
        let key = khz_key(freq_hz);
        self.skips.insert(key, (now, kind, 0));
        self.dirty = true;
    }

    /// Record a failed test. Consecutive failures escalate: after
    /// `persistent_after` strikes (counted across a ±1 kHz neighbourhood, so
    /// centroid jitter cannot dodge the count) the carrier is judged a
    /// birdie and parked for the long stretch.
    fn note_failure(&mut self, freq_hz: f64, now: f64) {
        let base = khz_key(freq_hz) as i64;
        // A repeat landing within ±1 kHz is the same carrier — centroid
        // jitter between passes — so it merges into the entry that is
        // already there rather than seeding a second one.
        let mut found: Option<u64> = None;
        let mut fails = 0;
        for d in -1..=1 {
            let k = (base + d).max(0) as u64;
            if let Some((_, kind, n)) = self.skips.get(&k) {
                if *kind == Skip::Persistent {
                    // Already parked long-term; keep its timer fresh.
                    self.skips.insert(k, (now, *kind, *n));
                    return;
                }
                if found.is_none() {
                    found = Some(k);
                }
                fails = fails.max(*n);
            }
        }
        let (key, kind, fails) = match found {
            Some(k) => {
                fails += 1;
                let kind = if self.cfg.persistent_after > 0
                    && fails as u32 >= self.cfg.persistent_after
                {
                    Skip::Persistent
                } else {
                    Skip::Failed
                };
                (k, kind, fails)
            }
            // A first failure counts as one, right away.
            None => (base.max(0) as u64, Skip::Failed, 1),
        };
        self.skips.insert(key, (now, kind, fails));
        self.dirty = true;
    }

    /// Seed the per-bin floor high so carriers stand above it from the first
    /// sweep frame, and quiet bins fall onto the real floor during Settle.
    /// (A min-statistics tracker seeded on the spectrum would let a steady
    /// carrier become its own floor — the exact failure the display floor
    /// avoids by having run forever before anyone looks at it.)
    fn seed_floor(&mut self, smoothed: &[f32]) {
        let seed = smoothed.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let seeded = vec![seed; smoothed.len()];
        self.floor.update(&seeded);
    }

    /// The one-line "state · progress" the worker's per-block classification
    /// stand-in shows while sweeping. Called at block rate, so it is the
    /// cheap subset of [`Self::status`]: no config clone, no skip-list
    /// build-and-sort, no per-slot Vecs — just the two strings the
    /// classification readout actually displays.
    pub fn progress_line(&self) -> String {
        let testing = self.slots.iter().flatten().any(|s| s.locked.is_none());
        let state = match (self.paused, self.state) {
            (true, _) => "paused",
            (false, State::Settle { .. }) => "settling",
            (false, State::Hunt) => {
                if self.listen().is_some() {
                    "locked"
                } else if testing {
                    "testing"
                } else {
                    "sweeping"
                }
            }
        };
        let busy = self.slots.iter().flatten().count();
        let mut progress = format!(
            "span {}/{} · slots {}/{}",
            self.span_index + 1,
            self.window_count().max(1),
            busy,
            self.slots.len().max(1)
        );
        let calls = self
            .slots
            .iter()
            .flatten()
            .filter(|s| s.locked.is_some())
            .count();
        if calls > 0 {
            progress.push_str(&format!(" · calls {calls}"));
        }
        if !self.cands.is_empty() {
            progress.push_str(&format!(" · {} candidates", self.cands.len()));
        }
        format!("{state} · {progress}")
    }

    pub fn status(&self, now: f64) -> ScanStatus {
        let locked: Vec<f64> = self.locked_freqs();
        // "Testing" is the slots still judging a candidate — a slot holding
        // a call is not under test any more, it is a call.
        let testing: Vec<f64> = self
            .slots
            .iter()
            .flatten()
            .filter(|s| s.locked.is_none())
            .map(|s| s.freq_hz)
            .collect();
        let (state, freq) = match (self.paused, self.state) {
            (true, _) => ("paused", self.tuned_hz),
            (false, State::Settle { .. }) => ("settling", self.tuned_hz),
            (false, State::Hunt) => {
                if let Some((i, _)) = self.listen() {
                    let f = self.slots[i].as_ref().map(|s| s.freq_hz).unwrap_or(0.0);
                    ("locked", f)
                } else if !testing.is_empty() {
                    ("testing", testing[0])
                } else {
                    ("sweeping", self.tuned_hz)
                }
            }
        };
        let n = self.window_count().max(1);
        let busy = self.slots.iter().flatten().count();
        let mut progress = format!(
            "span {}/{} · slots {}/{}",
            self.span_index + 1,
            n,
            busy,
            self.slots.len().max(1)
        );
        if !locked.is_empty() {
            progress.push_str(&format!(" · calls {}", locked.len()));
        }
        if !self.cands.is_empty() {
            progress.push_str(&format!(" · {} candidates", self.cands.len()));
        }
        ScanStatus {
            state: state.into(),
            freq_hz: freq,
            progress,
            candidates: self.cands.len(),
            testing,
            locked,
            hits: self.hits,
            skipped: self.skips.len(),
            cfg: self.cfg.clone(),
            message: self.msg.clone(),
            skip_list: self.skip_list(now),
        }
    }
}

/// The kHz key the skip memory and the slot-de-duplication use: candidates
/// carry sub-kHz centroid detail that neither needs.
fn khz_key(freq_hz: f64) -> u64 {
    (freq_hz / 1000.0).round() as u64
}

/// Judge one block of a slot's evidence against what a call looks like.
/// Digital needs frame sync (plus a call in progress, or enough control
/// traffic to condemn the channel); analog needs carrier AND voice.
fn judge_slot(slot: &mut Slot, dwell: f64, sig: Signal, block_s: f32) -> Verdict {
    let _ = slot.squelch.update(sig.snr_db, block_s);

    if let Some(d) = sig.digital {
        if d.locked {
            slot.locked_seen = true;
        }
        if d.control {
            slot.control_seen = true;
        }
        // Frame sync plus a call in progress is the whole argument; the
        // dwell exists for the cases that need longer to prove themselves.
        if d.in_call {
            return Verdict::Lock(d.mode);
        }
        // Locked onto a control channel: sync plus grant traffic and no
        // voice. It will still be saying nothing in a minute.
        if slot.locked_seen && slot.control_seen && dwell >= CONTROL_VERDICT_S {
            return Verdict::Control;
        }
    }

    if sig.nfm_voice_rms.is_some_and(|r| r > VOICE_RMS) {
        slot.nfm_voice_seen = true;
    }
    if sig.am_voice_rms.is_some_and(|r| r > VOICE_RMS) {
        slot.am_voice_seen = true;
    }

    // Analog: carrier AND voice. Locking on the squelch alone would file
    // every unmodulated carrier and rush of static as a call.
    if dwell >= ANALOG_MIN_S
        && slot.squelch.is_open()
        && (slot.nfm_voice_seen || slot.am_voice_seen)
    {
        let mode = if slot.nfm_voice_seen {
            ScanMode::Nfm
        } else {
            ScanMode::Am
        };
        return Verdict::Lock(mode);
    }

    if dwell >= TEST_DWELL_S {
        if slot.locked_seen {
            if slot.control_seen {
                return Verdict::Control;
            }
            return Verdict::Fail(Some("locked, no voice".into()));
        }
        Verdict::Fail(None)
    } else {
        Verdict::Keep
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic harness: a synthetic spectrum, a clock the tests
    /// advance by hand, and the actions the scanner asks for.
    struct Rig {
        scanner: Scanner,
        now: f64,
        spectrum: Vec<f32>,
        tuned: f64,
        actions: Vec<Action>,
    }

    const RATE: f64 = 2_048_000.0;
    const BINS: usize = 1024;
    const QUIET_DB: f32 = -60.0;

    impl Rig {
        fn new(cfg: ScanCfg) -> Self {
            // The rig's radio sits on the first window of the range, which is
            // where the scanner itself would put it.
            let cfg = cfg.normalized();
            let tuned = cfg.start_hz + RATE / 2.0;
            let mut r = Self {
                scanner: Scanner::new(cfg),
                now: 0.0,
                spectrum: vec![QUIET_DB; BINS],
                tuned,
                actions: Vec::new(),
            };
            // One frame to establish geometry (seeds the floor, starts Settle).
            r.frame();
            r
        }

        /// Advance one display frame (25 fps) and collect the actions.
        fn frame(&mut self) {
            self.now += 0.04;
            let acts = self.scanner.observe_frame(
                self.now,
                self.tuned,
                RATE,
                RATE * 0.49,
                &self.spectrum,
                &[],
            );
            self.actions.extend(acts);
        }

        fn settle(&mut self) {
            for _ in 0..(SETTLE_S / 0.04) as usize + 1 {
                self.frame();
            }
        }

        /// Deliver receiver evidence for one block to every running slot
        /// (idle slots get none). The scanner treats blocks as ~21 ms at a
        /// 48 kHz chain; the exact value only matters to the squelch's hang
        /// timer.
        fn signal(&mut self, sig: Signal) {
            self.now += 0.021;
            let n = self.scanner.slots.len();
            let sigs: Vec<Option<Signal>> = (0..n)
                .map(|i| self.scanner.slots[i].is_some().then_some(sig))
                .collect();
            let acts = self.scanner.note_signals(self.now, &sigs, 0.021);
            self.actions.extend(acts);
        }

        /// Deliver evidence to one slot only — the other running slots see
        /// an empty block.
        fn signal_slot(&mut self, slot: usize, sig: Signal) {
            self.signal_slots(&[(slot, sig)]);
        }

        /// Deliver evidence to several slots in ONE block — this is what
        /// makes simultaneous locks arbitrate against each other.
        fn signal_slots(&mut self, entries: &[(usize, Signal)]) {
            self.now += 0.021;
            let n = self.scanner.slots.len();
            let mut sigs: Vec<Option<Signal>> = vec![None; n];
            for (i, sig) in entries {
                if *i < n {
                    sigs[*i] = Some(*sig);
                }
            }
            let acts = self.scanner.note_signals(self.now, &sigs, 0.021);
            self.actions.extend(acts);
        }

        fn raise_carrier(&mut self, freq_hz: f64, snr_db: f32) {
            let bin_hz = RATE / BINS as f64;
            let centre_bin = ((freq_hz - self.tuned) / bin_hz + BINS as f64 / 2.0) as usize;
            // A tapered peak, not a plateau: the candidate finder keeps only
            // local maxima, and a flat top has no maximum in it — exactly why
            // real carriers (with skirts) are found and idealised blocks of
            // equal bins would not be.
            for (off, level) in [(-3, 8.0f32), (-2, 12.0), (-1, 16.0), (0, snr_db), (1, 16.0), (2, 12.0), (3, 8.0)] {
                let b = centre_bin.saturating_add_signed(off);
                if b < BINS {
                    self.spectrum[b] = QUIET_DB + level;
                }
            }
        }

        #[allow(dead_code)]
        fn quiet(&mut self) {
            self.spectrum.iter_mut().for_each(|v| *v = QUIET_DB);
        }

        fn last(&self) -> &Action {
            self.actions.last().expect("expected an action")
        }

        fn inspect_target(&self) -> f64 {
            match self.actions.last() {
                Some(Action::Inspect { freq_hz, .. }) => *freq_hz,
                other => panic!("expected Inspect, got {other:?}"),
            }
        }

        /// Every frequency the scanner asked a slot to listen to.
        fn inspect_freqs(&self) -> Vec<f64> {
            self.actions
                .iter()
                .filter_map(|a| match a {
                    Action::Inspect { freq_hz, .. } => Some(*freq_hz),
                    _ => None,
                })
                .collect()
        }
    }

    fn quiet_signal() -> Signal {
        Signal::default()
    }

    fn cfg_range(start: f64, stop: f64) -> ScanCfg {
        ScanCfg {
            start_hz: start,
            stop_hz: stop,
            ..ScanCfg::default()
        }
    }

    #[test]
    fn a_carrier_in_range_becomes_a_test_candidate() {
        let mut r = Rig::new(cfg_range(144_000_000.0, 148_000_000.0));
        r.settle();
        r.raise_carrier(145_500_000.0, 20.0);
        r.frame();
        let f = r.inspect_target();
        assert!(
            (f - 145_500_000.0).abs() < 5_000.0,
            "candidate should land on the carrier, got {f}"
        );
        assert_eq!(r.scanner.status(r.now).state, "testing");
    }

    #[test]
    fn a_carrier_outside_the_range_is_ignored() {
        let mut r = Rig::new(cfg_range(144_000_000.0, 148_000_000.0));
        r.settle();
        r.raise_carrier(142_100_000.0, 30.0);
        for _ in 0..30 {
            r.frame();
        }
        assert!(
            r.actions.iter().all(|a| !matches!(a, Action::Inspect { .. })),
            "nothing in range, so nothing to inspect"
        );
    }

    /// The peak detector's floor is seeded high and needs its settle time;
    /// before that, no candidate may be produced off an unjudged spectrum.
    #[test]
    fn no_candidates_until_settled() {
        let mut r = Rig::new(cfg_range(144_000_000.0, 148_000_000.0));
        r.raise_carrier(145_500_000.0, 20.0);
        // Frames before SETTLE_S has elapsed.
        for _ in 0..10 {
            r.frame();
        }
        assert!(
            r.actions.iter().all(|a| !matches!(a, Action::Inspect { .. })),
            "the floor is still falling; judging now would be noise"
        );
    }

    #[test]
    fn a_digital_call_locks_immediately_and_holds_then_resumes() {
        let mut r = Rig::new(cfg_range(144_000_000.0, 148_000_000.0));
        r.settle();
        r.raise_carrier(145_500_000.0, 20.0);
        r.frame();
        let f = r.inspect_target();

        let d = |in_call: bool| Signal {
            snr_db: 18.0,
            digital: Some(DigitalEvidence {
                mode: ScanMode::P25,
                locked: true,
                in_call,
                control: false,
            }),
            ..Signal::default()
        };

        r.signal(d(true));
        assert!(r.scanner.is_locked(), "in-call is a lock");
        assert_eq!(r.scanner.locked_mode(), Some(ScanMode::P25));
        assert_eq!(r.scanner.status(r.now).state, "locked");

        // The call continues: still locked, and the lock survives gaps shorter
        // than the resume delay.
        for _ in 0..50 {
            r.signal(d(true));
        }
        assert!(r.scanner.is_locked());

        // The call drops; the resume delay must pass before the sweep moves.
        for _ in 0..40 {
            r.signal(d(false));
        }
        assert!(r.scanner.is_locked(), "resume delay holds the lock");
        for _ in 0..150 {
            r.signal(d(false));
        }
        assert!(!r.scanner.is_locked(), "resume delay expired; sweep resumes");
        assert_eq!(r.scanner.status(r.now).state, "sweeping");
        assert!(
            (f - 145_500_000.0).abs() < 5_000.0,
            "sanity: was testing the carrier"
        );
    }

    /// Two carriers in the window are put under test at the same time — the
    /// whole point of the slot pool. A birdie's dwell no longer serialises
    /// in front of the carrier next to it.
    #[test]
    fn two_carriers_are_tested_in_parallel() {
        let mut r = Rig::new(cfg_range(144_000_000.0, 148_000_000.0));
        r.settle();
        r.raise_carrier(145_100_000.0, 18.0);
        r.raise_carrier(145_900_000.0, 24.0);
        r.frame();
        let freqs = r.inspect_freqs();
        assert_eq!(freqs.len(), 2, "both carriers go under test at once");
        assert!(
            freqs.iter().all(|f| (f - 145_100_000.0).abs() < 10_000.0
                || (f - 145_900_000.0).abs() < 10_000.0),
            "both on the raised carriers, got {freqs:?}"
        );
        assert_eq!(r.scanner.status(r.now).testing.len(), 2);
        // The stronger carrier took the first slot.
        let freqs = r.inspect_freqs();
        assert!(
            (freqs[0] - 145_900_000.0).abs() < 10_000.0,
            "strongest first: slot 0 got the 24 dB carrier, got {freqs:?}"
        );
    }

    /// When two slots prove a call in the same block, BOTH hold: the pool
    /// is not torn down, each call records, and the stronger carrier takes
    /// the speaker.
    #[test]
    fn two_simultaneous_calls_both_hold_and_the_stronger_takes_the_speaker() {
        let mut r = Rig::new(cfg_range(144_000_000.0, 148_000_000.0));
        r.settle();
        r.raise_carrier(145_200_000.0, 15.0);
        r.raise_carrier(145_800_000.0, 30.0);
        r.frame();
        assert_eq!(r.inspect_freqs().len(), 2, "both under test");

        let d = |in_call: bool, snr: f32| Signal {
            snr_db: snr,
            digital: Some(DigitalEvidence {
                mode: ScanMode::P25,
                locked: true,
                in_call,
                control: false,
            }),
            ..Signal::default()
        };
        // Slot 0 holds the stronger carrier (strongest-first assignment).
        r.signal_slots(&[(1, d(true, 10.0)), (0, d(true, 20.0))]);
        assert!(r.scanner.is_locked());
        let status = r.scanner.status(r.now);
        assert_eq!(status.locked.len(), 2, "both calls are held");
        assert_eq!(status.testing.len(), 0, "neither is under test any more");
        assert_eq!(status.state, "locked");
        assert!(
            (status.freq_hz - 145_800_000.0).abs() < 5_000.0,
            "the stronger carrier takes the speaker, got {}",
            status.freq_hz
        );
        assert!(
            !r.actions.iter().any(|a| matches!(a, Action::Release { .. })),
            "no slot was torn down — both calls keep recording"
        );
    }

    /// A held call does not stop the sweep: idle slots keep taking new
    /// candidates while a call records.
    #[test]
    fn a_held_call_does_not_stop_the_pool() {
        let mut r = Rig::new(cfg_range(144_000_000.0, 148_000_000.0));
        r.settle();
        r.raise_carrier(145_100_000.0, 20.0);
        r.frame();
        let d = |in_call: bool| Signal {
            snr_db: 18.0,
            digital: Some(DigitalEvidence {
                mode: ScanMode::P25,
                locked: true,
                in_call,
                control: false,
            }),
            ..Signal::default()
        };
        r.signal(d(true));
        assert!(r.scanner.is_locked());
        // A second carrier appears while the first call is held.
        r.raise_carrier(145_900_000.0, 22.0);
        r.frame();
        assert!(
            r.actions.iter().any(|a| matches!(a, Action::Inspect { freq_hz, .. } if (*freq_hz - 145_900_000.0).abs() < 10_000.0)),
            "the pool keeps testing while a call is held"
        );
    }

    /// The window cannot advance while any call is held: a slot receiver
    /// only sees its channel inside the current span.
    #[test]
    fn the_window_is_pinned_while_calls_are_held() {
        let start = 144_000_000.0;
        let mut r = Rig::new(cfg_range(start, start + 8_000_000.0));
        r.settle();
        r.raise_carrier(145_100_000.0, 20.0);
        r.frame();
        let d = |in_call: bool| Signal {
            snr_db: 18.0,
            digital: Some(DigitalEvidence {
                mode: ScanMode::P25,
                locked: true,
                in_call,
                control: false,
            }),
            ..Signal::default()
        };
        r.signal(d(true));
        assert!(r.scanner.is_locked());
        r.quiet();
        // Far past the sweep-idle linger an empty window would have advanced.
        for _ in 0..80 {
            r.frame();
            r.signal(d(true));
        }
        assert!(
            r.actions.iter().all(|a| !matches!(a, Action::TuneSpan(_))),
            "a held call pins the tuner to its window"
        );
        assert_eq!(r.scanner.status(r.now).state, "locked");
    }

    /// When the call on the speaker ends, the speaker falls to the newest
    /// call still held instead of sandwiching the overlap in silence.
    #[test]
    fn the_speaker_falls_to_a_call_still_held() {
        let mut r = Rig::new(cfg_range(144_000_000.0, 148_000_000.0));
        r.settle();
        r.raise_carrier(145_200_000.0, 15.0);
        r.raise_carrier(145_800_000.0, 30.0);
        r.frame();
        let _ = r.inspect_freqs();
        let d = |in_call: bool, snr: f32| Signal {
            snr_db: snr,
            digital: Some(DigitalEvidence {
                mode: ScanMode::P25,
                locked: true,
                in_call,
                control: false,
            }),
            ..Signal::default()
        };
        r.signal_slots(&[(0, d(true, 20.0)), (1, d(true, 12.0))]);
        assert_eq!(r.scanner.locked_freqs().len(), 2);
        assert!(
            (r.scanner.status(r.now).freq_hz - 145_800_000.0).abs() < 5_000.0,
            "the stronger call (slot 0) is on the speaker"
        );
        // Slot 0's call drops; slot 1's call is still going.
        for _ in 0..200 {
            r.signal_slots(&[(0, d(false, 20.0)), (1, d(true, 12.0))]);
        }
        let status = r.scanner.status(r.now);
        assert_eq!(status.locked.len(), 1, "one call is still held");
        assert!(
            (status.freq_hz - 145_200_000.0).abs() < 5_000.0,
            "the speaker fell to the call still on the air, got {}",
            status.freq_hz
        );
    }

    /// Two held calls end independently: each releases on its own resume
    /// delay, and the window only moves once the last one has dropped.
    #[test]
    fn held_calls_release_independently() {
        let start = 144_000_000.0;
        let mut r = Rig::new(cfg_range(start, start + 8_000_000.0));
        r.settle();
        r.raise_carrier(145_200_000.0, 15.0);
        r.raise_carrier(145_800_000.0, 30.0);
        r.frame();
        let _ = r.inspect_freqs();
        let d = |in_call: bool, snr: f32| Signal {
            snr_db: snr,
            digital: Some(DigitalEvidence {
                mode: ScanMode::P25,
                locked: true,
                in_call,
                control: false,
            }),
            ..Signal::default()
        };
        r.signal_slots(&[(0, d(true, 20.0)), (1, d(true, 12.0))]);
        // Slot 0 drops; slot 1 holds well past the sweep-idle linger.
        r.quiet();
        for _ in 0..200 {
            r.frame();
            r.signal_slots(&[(0, d(false, 20.0)), (1, d(true, 12.0))]);
        }
        assert!(r.scanner.is_locked(), "the second call is still held");
        assert!(
            r.actions.iter().all(|a| !matches!(a, Action::TuneSpan(_))),
            "still pinned by the remaining call"
        );
        // Now the last call drops — with no sweep frames, so the empty
        // window cannot advance and prune the released memories before
        // this reads them.
        for _ in 0..200 {
            r.signal_slots(&[(1, d(false, 12.0))]);
        }
        assert!(!r.scanner.is_locked());
        assert_eq!(r.scanner.skip_list(r.now).len(), 2, "both released");
    }

    /// A control channel: sync and grants, never a call. Parked out for a
    /// long time, with the skip visible in the status.
    #[test]
    fn a_control_channel_is_parked_out() {
        let mut r = Rig::new(cfg_range(144_000_000.0, 148_000_000.0));
        r.settle();
        r.raise_carrier(145_500_000.0, 25.0);
        r.frame();
        let _ = r.inspect_target();

        let d = |control: bool| Signal {
            snr_db: 22.0,
            digital: Some(DigitalEvidence {
                mode: ScanMode::P25,
                locked: true,
                in_call: false,
                control,
            }),
            ..Signal::default()
        };

        // Locked with grants and no voice: skip as soon as the early verdict
        // window has passed.
        for _ in 0..10 {
            r.signal(d(true));
        }
        for _ in 0..40 {
            r.signal(d(true));
        }
        assert!(!r.scanner.is_locked(), "a control channel is not a hit");
        assert!(
            r.actions
                .iter()
                .any(|a| matches!(a, Action::Notice(m) if m.contains("control"))),
            "the skip should be explained in the log"
        );
        assert!(r.scanner.status(r.now).skipped > 0);

        // And the same spectrum must not immediately re-pick it: the next
        // sweep frame produces no Inspect for that frequency.
        r.quiet();
        let before = r.actions.len();
        for _ in 0..10 {
            r.frame();
        }
        assert!(
            r.actions[before..]
                .iter()
                .all(|a| !matches!(a, Action::Inspect { freq_hz, .. } if (*freq_hz - 145_500_000.0).abs() < 10_000.0)),
            "a parked-out control channel is not retested on the next pass"
        );
    }

    #[test]
    fn an_analog_channel_needs_squelch_and_voice() {
        let mut r = Rig::new(cfg_range(144_000_000.0, 148_000_000.0));
        r.settle();
        r.raise_carrier(145_900_000.0, 20.0);
        r.frame();
        let _ = r.inspect_target();

        // A strong carrier with gated silence: not a call yet.
        let s = |rms: f32| Signal {
            snr_db: 16.0,
            nfm_voice_rms: Some(rms),
            ..Signal::default()
        };
        for _ in 0..5 {
            r.signal(s(0.001));
        }
        assert!(!r.scanner.is_locked(), "carrier alone is not voice");
        // Voice arrives: squelch is open, energy is there — lock.
        for _ in 0..8 {
            r.signal(s(0.15));
        }
        assert!(r.scanner.is_locked());
        assert_eq!(r.scanner.locked_mode(), Some(ScanMode::Nfm));
        let rec = |sc: &Scanner| sc.recording_states().first().copied().flatten();
        assert!(
            rec(&r.scanner).is_some_and(|s| s.analog_open),
            "analog locks are recorded"
        );

        // The squelch closes (SNR collapses). The hang bridges short gaps, so
        // the recording stops only once the hang has expired; the release
        // follows after that plus the resume delay.
        let quiet = Signal {
            snr_db: 0.0,
            ..Signal::default()
        };
        for _ in 0..90 {
            r.signal(quiet);
        }
        assert!(
            rec(&r.scanner).is_none_or(|s| !s.analog_open),
            "the squelch hang expired; nothing more to record"
        );
        for _ in 0..200 {
            r.signal(quiet);
        }
        assert!(!r.scanner.is_locked());
    }

    /// An unmodulated carrier must not become an analog hit at dwell expiry,
    /// no matter how strong.
    #[test]
    fn dwell_expiry_without_voice_is_rejected() {
        let mut r = Rig::new(cfg_range(144_000_000.0, 148_000_000.0));
        r.settle();
        r.raise_carrier(145_900_000.0, 20.0);
        r.frame();
        let _ = r.inspect_target();
        let s = Signal {
            snr_db: 16.0,
            nfm_voice_rms: Some(0.001),
            ..Signal::default()
        };
        for _ in 0..(TEST_DWELL_S / 0.021) as usize + 2 {
            r.signal(s);
        }
        assert!(!r.scanner.is_locked());
        assert_eq!(r.scanner.status(r.now).state, "sweeping");
    }

    /// An empty window: the scanner lingers briefly, then moves the span
    /// onward toward the top of the range.
    #[test]
    fn an_empty_window_advances_the_span() {
        let start = 144_000_000.0;
        let mut r = Rig::new(cfg_range(start, start + 8_000_000.0));
        r.settle();
        r.quiet();
        let before = r.actions.len();
        // Settle + sweep-idle + margin.
        for _ in 0..40 {
            r.frame();
        }
        match r.actions[before..].iter().find(|a| matches!(a, Action::TuneSpan(_))) {
            Some(Action::TuneSpan(centre)) => {
                assert!(
                    *centre > r.tuned,
                    "the next window is further up the range"
                );
                assert!(
                    *centre - RATE / 2.0 >= start - 1.0,
                    "the window still covers the start of the range"
                );
            }
            other => panic!("expected a span move, got {other:?}"),
        }
        assert_eq!(r.scanner.status(r.now).state, "settling");
    }

    /// A range narrower than one span never asks the tuner to move.
    #[test]
    fn a_range_within_one_span_stays_put() {
        let mut r = Rig::new(cfg_range(144_000_000.0, 145_000_000.0));
        r.settle();
        r.quiet();
        for _ in 0..60 {
            r.frame();
        }
        assert!(
            r.actions.iter().all(|a| !matches!(a, Action::TuneSpan(_))),
            "one window covers it; no tuner writes"
        );
    }

    #[test]
    fn pausing_freezes_everything_until_resumed() {
        let mut r = Rig::new(cfg_range(144_000_000.0, 148_000_000.0));
        r.settle();
        r.raise_carrier(145_500_000.0, 20.0);
        r.frame();
        r.scanner.pause("test");
        let before = r.actions.len();
        r.signal(quiet_signal());
        r.frame();
        r.signal(quiet_signal());
        assert_eq!(r.actions.len(), before, "a paused scanner acts on nothing");
        assert_eq!(r.scanner.status(r.now).state, "paused");

        r.scanner.resume();
        assert_eq!(r.scanner.status(r.now).state, "settling");
    }

    /// With one slot the sweep traverses candidates in order, skipping what
    /// the operator sent it past.
    #[test]
    fn skipped_button_advances_past_the_candidate() {
        let mut r = Rig::new(ScanCfg {
            slots: 1,
            ..cfg_range(144_000_000.0, 148_000_000.0)
        });
        r.settle();
        r.raise_carrier(145_100_000.0, 20.0);
        r.raise_carrier(145_900_000.0, 20.0);
        r.frame();
        let first = r.inspect_target();
        r.actions.clear();
        for a in r.scanner.skip(r.now) {
            r.actions.push(a);
        }
        // The sweep restarts on the next display frame, which now skips the
        // parked-out candidate and picks the one above it.
        r.frame();
        let second = r.inspect_target();
        assert!(
            second > first,
            "skip moves to the next candidate up the band"
        );
        assert_eq!(r.scanner.status(r.now).state, "testing");
    }

    /// With the default pool, the operator's skip clears every slot at once:
    /// whatever each of them was sitting through is left behind together.
    #[test]
    fn skipping_with_a_full_pool_clears_every_slot() {
        let mut r = Rig::new(cfg_range(144_000_000.0, 148_000_000.0));
        r.settle();
        r.raise_carrier(145_100_000.0, 20.0);
        r.raise_carrier(145_500_000.0, 20.0);
        r.raise_carrier(145_900_000.0, 20.0);
        r.frame();
        assert_eq!(r.inspect_freqs().len(), 3, "the pool took all three");
        let before = r.actions.len();
        for a in r.scanner.skip(r.now) {
            r.actions.push(a);
        }
        assert_eq!(r.scanner.status(r.now).testing.len(), 0, "pool emptied");
        assert_eq!(
            r.actions[before..]
                .iter()
                .filter(|a| matches!(a, Action::Release { .. }))
                .count(),
            3,
            "the worker is told to drop all three slots"
        );
        assert!(r.scanner.status(r.now).skipped >= 3, "all remembered");
    }

    /// A released channel must not be re-picked the moment the sweep
    /// restarts, even though it is still the strongest thing on the band.
    #[test]
    fn a_released_channel_is_not_immediately_relocked() {
        let mut r = Rig::new(cfg_range(144_000_000.0, 148_000_000.0));
        r.settle();
        r.raise_carrier(145_500_000.0, 20.0);
        r.frame();
        let _ = r.inspect_target();
        let d = Signal {
            snr_db: 18.0,
            digital: Some(DigitalEvidence {
                mode: ScanMode::P25,
                locked: true,
                in_call: true,
                control: false,
            }),
            ..Signal::default()
        };
        r.signal(d);
        for _ in 0..200 {
            r.signal(Signal {
                snr_db: 18.0,
                digital: Some(DigitalEvidence {
                    mode: ScanMode::P25,
                    locked: true,
                    in_call: false,
                    control: false,
                }),
                ..Signal::default()
            });
        }
        assert!(!r.scanner.is_locked(), "released");
        // The carrier is still on the air. The next sweep frame may see it,
        // but the revisit memory keeps the scanner from re-locking it now.
        let before = r.actions.len();
        for _ in 0..20 {
            r.frame();
        }
        assert!(
            r.actions[before..]
                .iter()
                .all(|a| !matches!(a, Action::Inspect { freq_hz, .. } if (*freq_hz - 145_500_000.0).abs() < 10_000.0)),
            "just-released traffic is left alone for a while"
        );
    }

    #[test]
    fn configuration_changes_are_normalised_and_applied() {
        let mut r = Rig::new(cfg_range(148_000_000.0, 144_000_000.0));
        assert!(r.scanner.cfg().start_hz < r.scanner.cfg().stop_hz);

        r.settle();
        let _ = r.scanner.set_cfg(ScanCfg {
            start_hz: 440_000_000.0,
            stop_hz: 442_000_000.0,
            modes: vec![],
            ..ScanCfg::default()
        });
        assert!(
            r.scanner.cfg().modes.contains(&ScanMode::Nfm),
            "an empty mode list falls back to the default set"
        );
        assert_eq!(r.scanner.status(r.now).state, "settling");
    }

    /// Reconfiguring to a range the radio cannot see must move the tuner to
    /// the new range before sweeping — not spend sweeps on a span where no
    /// candidate can ever pass the range filter.
    #[test]
    fn reconfiguring_outside_the_current_span_moves_the_tuner() {
        let mut r = Rig::new(cfg_range(144_000_000.0, 148_000_000.0));
        r.settle();
        r.frame(); // one sweep frame on the original range
        r.actions.clear();

        // Airband: nowhere near the 2 m band the radio is sitting in.
        let _ = r.scanner.set_cfg(cfg_range(121_000_000.0, 127_000_000.0));
        for _ in 0..60 {
            r.frame();
        }
        match r
            .actions
            .iter()
            .find(|a| matches!(a, Action::TuneSpan(_)))
        {
            Some(Action::TuneSpan(centre)) => {
                assert!(
                    *centre > 121_000_000.0 && *centre < 127_000_000.0,
                    "the tuner lands inside the new range, at {centre}"
                );
            }
            other => panic!("expected a span move to the new range, got {other:?}"),
        }
    }

    /// Reconfiguring to a range that still contains the current window must
    /// NOT force a tuner move — the sweep continues where it is. (The window
    /// may still advance later on an empty sweep, which is ordinary
    /// progress, not a reseat.)
    #[test]
    fn reconfiguring_within_the_current_span_stays_put() {
        let mut r = Rig::new(cfg_range(144_000_000.0, 148_000_000.0));
        r.settle();
        r.frame();
        r.actions.clear();

        // The radio sits at 145.024 MHz; this range still covers it. Thirty
        // frames is past the settle but before the empty-window advance.
        let _ = r.scanner.set_cfg(cfg_range(143_000_000.0, 147_000_000.0));
        for _ in 0..30 {
            r.frame();
        }
        assert!(
            r.actions.iter().all(|a| !matches!(a, Action::TuneSpan(_))),
            "no forced tuner move when the window still sees the range"
        );
    }

    #[test]
    fn the_skip_memory_expires() {
        let mut r = Rig::new(cfg_range(144_000_000.0, 148_000_000.0));
        r.settle();
        r.raise_carrier(145_500_000.0, 20.0);
        r.frame();
        let f = r.inspect_target();
        r.actions.clear();
        for a in r.scanner.skip(r.now) {
            r.actions.push(a);
        }
        assert!(
            r.scanner
                .skipped_at(f, r.now + FAILED_RETEST_S - 1.0)
                .is_some(),
            "still remembered just before the TTL"
        );
        assert!(
            r.scanner.skipped_at(f, r.now + FAILED_RETEST_S + 1.0).is_none(),
            "forgotten after the TTL, so the band gets re-tested"
        );
    }

    /// A carrier that keeps failing is a birdie: after the configured
    /// strikes it parks for the long stretch, and one re-test clears it.
    #[test]
    fn repeated_failures_escalate_to_a_persistent_park() {
        let mut r = Rig::new(ScanCfg {
            persistent_after: 3,
            persistent_skip_s: 86_400,
            ..cfg_range(144_000_000.0, 148_000_000.0)
        });
        r.settle();
        let f = 145_500_000.0;
        // Two strikes, one of them jittered within the ±1 kHz merge window.
        r.scanner.note_failure(f, r.now);
        r.now += 1.0;
        r.scanner.note_failure(f + 700.0, r.now);
        let list = r.scanner.skip_list(r.now);
        assert_eq!(list.len(), 1, "jittered keys merge into one entry");
        assert_eq!(list[0].fails, 2, "neighbourhood fails merge");
        assert_eq!(list[0].reason, "no decode");
        // Third strike: persistent, parked for about a day.
        r.now += 1.0;
        r.scanner.note_failure(f, r.now);
        let list = r.scanner.skip_list(r.now);
        assert_eq!(list[0].reason, "persistent noise");
        assert!(list[0].expires_in_s >= 86_390, "parked for the long stretch");
        // One re-test clears even a persistent park.
        r.scanner.unskip(f);
        assert!(r.scanner.skip_list(r.now).is_empty());
    }

    /// The escalation is a knob, not a law: zero strikes disables it.
    #[test]
    fn persistent_escalation_can_be_disabled() {
        let mut r = Rig::new(ScanCfg {
            persistent_after: 0,
            ..cfg_range(144_000_000.0, 148_000_000.0)
        });
        for i in 0..6 {
            r.scanner.note_failure(145_500_000.0, r.now + i as f64);
        }
        let list = r.scanner.skip_list(r.now);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].reason, "no decode", "never escalates");
    }

    /// The panel can see what was skipped, why, and un-skip it: the next
    /// sweep frame re-picks the frequency instead of waiting out the timer.
    #[test]
    fn the_skip_list_is_visible_and_unskip_works() {
        let mut r = Rig::new(cfg_range(144_000_000.0, 148_000_000.0));
        r.settle();
        r.raise_carrier(145_500_000.0, 20.0);
        r.frame();
        let f = r.inspect_target();
        r.actions.clear();
        for a in r.scanner.skip(r.now) {
            r.actions.push(a);
        }
        let list = r.scanner.skip_list(r.now);
        assert_eq!(list.len(), 1);
        assert!((list[0].freq_hz - f).abs() < 2_000.0, "the entry names the frequency");
        assert_eq!(list[0].reason, "no decode");

        r.scanner.unskip(f);
        assert!(r.scanner.skip_list(r.now).is_empty(), "unskipped");
        // The sweep picks it straight back up rather than waiting the TTL.
        r.frame();
        assert!(
            r.actions.iter().any(|a| matches!(a, Action::Inspect { freq_hz, .. } if (*freq_hz - f).abs() < 10_000.0)),
            "unskipped frequency is re-tested immediately"
        );
    }

    /// A released call comes back quickly (the listener wants the next
    /// call), while a control channel parks for its configured stretch.
    #[test]
    fn released_and_control_skips_have_their_own_timers() {
        let mut r = Rig::new(cfg_range(144_000_000.0, 148_000_000.0));
        r.settle();
        r.raise_carrier(145_500_000.0, 20.0);
        r.frame();
        let _ = r.inspect_target();
        let d = Signal {
            snr_db: 18.0,
            digital: Some(DigitalEvidence {
                mode: ScanMode::P25,
                locked: true,
                in_call: true,
                control: false,
            }),
            ..Signal::default()
        };
        r.signal(d);
        for _ in 0..200 {
            r.signal(Signal {
                snr_db: 18.0,
                digital: Some(DigitalEvidence {
                    mode: ScanMode::P25,
                    locked: true,
                    in_call: false,
                    control: false,
                }),
                ..Signal::default()
            });
        }
        assert!(!r.scanner.is_locked(), "released");
        let list = r.scanner.skip_list(r.now);
        assert_eq!(list.len(), 1, "the released channel is remembered");
        assert_eq!(list[0].reason, "recently held");
        assert!(
            list[0].expires_in_s <= RELEASED_REVISIT_S as u32,
            "a released channel is only parked for the short release window"
        );
    }
}
