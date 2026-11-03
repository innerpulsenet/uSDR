//! The application: the connection, the receiver model it keeps current, and
//! the window it lays out — masthead, VFO bar, panafall, applet column,
//! dock and status bar.

use crate::audio::Player;
use crate::brand;
use crate::meter::Meter;
use crate::net::{Fetch, Get, Link, NetEvent};
use crate::prefs::{Memory, Motion, Prefs};
use crate::scope::Scope;
use crate::spectrum::{Act, Panafall, SliceFlag};
use crate::theme::{self, alpha};
use crate::vfo::Vfo;
use crate::widgets::{self, Tone};
use egui::{Align2, CornerRadius, Rect, Sense, Stroke, Ui, pos2, vec2};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};
use usdr_proto::{self as proto, Call, Classification, Device, FftFrame, Mode, ModeCaps, Presence, Status};

pub enum Conn {
    Idle,
    Connecting { attempt: u32, reason: Option<String> },
    Live,
}

pub struct DecodeRow {
    pub at: SystemTime,
    pub freq_hz: f64,
    pub ev: proto::DecodeEvent,
    /// Its fields shown inline under the row.
    pub open: bool,
    /// Protocol, kind and summary in lower case, for the filter.
    pub key: String,
    /// Its height in the log when last laid out; 0 until it has been.
    pub height: f32,
    /// When the log first showed it, for its arrival flash.
    pub born: Option<f64>,
}

impl DecodeRow {
    pub fn new(at: SystemTime, freq_hz: f64, ev: proto::DecodeEvent) -> Self {
        let key = format!("{} {} {}", ev.protocol, ev.kind, ev.summary).to_lowercase();
        Self { at, freq_hz, ev, open: false, key, height: 0.0, born: None }
    }
}

/// A recording playing in place of the live audio.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Playing {
    Call(u64),
    Capture,
}

pub struct ClassRow {
    pub at: SystemTime,
    pub freq_hz: f64,
    pub c: Classification,
}

/// What a file dialog was opened for, with what it returned.
pub enum Picked {
    Replay(Vec<std::path::PathBuf>),
    RecordDir(std::path::PathBuf),
}

/// Open a native file dialog without stalling the window: it runs on its own
/// thread and its answer arrives through `App::picked`.
pub fn pick(app: &App, what: &'static str) {
    let tx = app.picked.0.clone();
    let start = app.prefs.record_dir();
    let _ = std::thread::Builder::new().name("usdr-dialog".into()).spawn(move || {
        let d = rfd::FileDialog::new().set_directory(&start);
        let out = match what {
            "replay" => d
                .set_title("Decode a capture")
                .add_filter("I/Q capture", &["iq", "json", "wav"])
                .pick_files()
                .map(Picked::Replay),
            _ => d.set_title("Save recordings in").pick_folder().map(Picked::RecordDir),
        };
        if let Some(p) = out {
            let _ = tx.send(p);
        }
    });
}

pub struct Toast {
    pub text: String,
    pub tone: Tone,
    pub until: f64,
    /// When it appeared, for its rise; its height once drawn, for stacking.
    pub born: f64,
    pub height: f32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DockTab {
    Decodes,
    Classifier,
    Calls,
    Scan,
}

/// The spur check walks three steps a second or so apart.
pub enum SpurCheck {
    Settle { until: f64, home: f64 },
    Shifted { until: f64, home: f64, a: Vec<f32> },
    Back { until: f64, a: Vec<f32>, b: Vec<f32>, rate: f64 },
}

pub struct App {
    pub prefs: Prefs,
    pub link: Option<Link>,
    pub player: Arc<Player>,
    /// Receiver B's audio, on its own stream.
    pub player_b: Arc<Player>,
    /// Whether the display, the readout and the keys drive receiver B.
    pub active_b: bool,
    pub conn: Conn,
    pub status: Status,
    pub have_status: bool,
    pub modes: Vec<ModeCaps>,
    pub presence: Presence,
    /// What the server said it is; `None` for servers before API 2.
    pub server: Option<proto::ServerInfo>,
    pub memories: Option<Vec<Memory>>,
    pub frame: Option<Box<FftFrame>>,
    pub pan: Panafall,
    pub vfo: Vfo,
    pub meter: Meter,
    /// Receiver B's meter, measured from the spectrum over its passband.
    pub meter_b: Meter,
    /// Until when (ui time) a narrow SIGNAL bay keeps showing the carrier
    /// error after it last read over the warning level, so a reading that
    /// hovers at the threshold does not blink in and out.
    pub err_warn_until: f64,
    /// Receiver B's channel power in the newest frame, for its flag.
    level_b: Option<(f32, f32)>,
    pub scope: Scope,
    pub decodes: VecDeque<DecodeRow>,
    pub class_log: VecDeque<ClassRow>,
    last_class_key: Option<(String, Instant)>,
    pub held: Option<(Classification, Instant)>,
    pub calls: Vec<Call>,
    calls_polled: f64,
    pub devices: Vec<Device>,
    pub toasts: Vec<Toast>,
    pub dock_tab: DockTab,
    pub decode_filter: String,
    /// Rows that arrived since the log last drew, and where it was scrolled,
    /// so new frames arriving on top do not move what is being read.
    pub decodes_added: usize,
    pub decode_scroll: f32,
    /// Frames that arrived above the rows being read.
    pub decodes_unseen: usize,
    /// Scroll the log back to the newest frame on the next draw.
    pub decode_jump: bool,
    /// What the speaker is playing instead of live audio.
    pub playing: Option<Playing>,
    /// The channel's level and floor over the last 20 s (time, dBFS, dBFS),
    /// the span the server's capture buffer holds; cleared on a retune, as
    /// that buffer is.
    pub activity: VecDeque<(f64, f32, f32)>,
    activity_hz: f64,
    /// A memory slot being renamed, and the name so far.
    pub renaming: Option<(usize, String)>,
    /// Files picked in a dialog, arriving from its thread.
    pub picked: (std::sync::mpsc::Sender<Picked>, std::sync::mpsc::Receiver<Picked>),
    pub selected_decode: Option<usize>,
    pub scan_form: proto::ScanCfg,
    pub scan_touched: bool,
    pub p25: crate::panels::P25Form,
    pub settings_open: bool,
    pub help_open: bool,
    pub connect_input: String,
    pub splash: brand::Splash,
    /// Optimistic receive frequency until the server confirms it.
    pub inspect_pending: Option<(f64, Instant)>,
    pub gain_ui: f64,
    pub zoom_ui: f64,
    pub span_pending: Option<(f64, f64)>,
    pub clip_until: f64,
    pub spur: Option<SpurCheck>,
    display_bins_sent: usize,
    display_bins_due: Option<(usize, f64)>,
    // Rates for the status bar.
    rate_t: f64,
    rate_frames: u64,
    rate_bytes: u64,
    pub fps: f32,
    pub kbps: f32,
    /// The last two minutes of (fps, KiB/s, API ms), a sample a second.
    pub history: VecDeque<(f32, f32, f32)>,
    pub reconnects: u32,
    pub connected_at: Option<Instant>,
    pub diagnostics_open: bool,
    pub replay_note: Option<String>,
    pub recorder_busy: Option<String>,
    pub memory_armed: bool,
    /// Output devices for the settings list, looked for the first time it
    /// is shown (it takes ALSA tens of milliseconds) and on request after.
    pub audio_devices: Option<Vec<String>>,
    pub fullscreen: bool,
    /// The window is wide enough for the memories to ride on the toolbar.
    pub memories_on_toolbar: bool,
    /// The window title as last set.
    title: String,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, server: Option<String>) -> App {
        let prefs = Prefs::load();
        let player = Player::new(prefs.audio_device.as_deref());
        let player_b = Player::new(prefs.audio_device.as_deref());
        let mut app = App::build(&cc.egui_ctx, prefs, player, player_b, server.clone());
        if server.is_some() || app.prefs.auto_connect && app.prefs.last_server.is_some() {
            let s = app.connect_input.clone();
            app.connect(&cc.egui_ctx, &s);
        }
        app
    }

    /// Everything but connecting and the sound card, so tests can build one.
    pub fn build(ctx: &egui::Context, prefs: Prefs, player: Arc<Player>, player_b: Arc<Player>, server: Option<String>) -> App {
        theme::install(ctx, prefs.ground);
        ctx.set_zoom_factor(prefs.ui_scale.clamp(0.6, 2.0));
        crate::anim::set_max_fps(prefs.max_fps);
        player_b.set_volume(prefs.volume_b);
        player_b.set_muted(prefs.muted_b);
        player_b.set_pan(prefs.pan_b);
        player.set_volume(prefs.volume);
        player.set_muted(prefs.muted);
        player.set_pan(prefs.pan);
        // The A/B split carries each receiver's whole program in its ear.
        let split = prefs.pan <= -0.99 && prefs.pan_b >= 0.99;
        player.set_fold(split);
        player_b.set_fold(split);
        let connect_input = server
            .clone()
            .or_else(|| prefs.last_server.clone())
            .unwrap_or_else(|| "127.0.0.1:8073".into());
        let qa_tab = match std::env::var("USDR_QA_TAB").as_deref() {
            Ok("calls") => DockTab::Calls,
            Ok("scan") => DockTab::Scan,
            Ok("class") => DockTab::Classifier,
            _ => DockTab::Decodes,
        };
        App {
            prefs,
            link: None,
            player,
            player_b,
            active_b: false,
            conn: Conn::Idle,
            status: Status::default(),
            have_status: false,
            modes: Vec::new(),
            presence: Presence::default(),
            server: None,
            memories: None,
            frame: None,
            pan: Panafall::default(),
            vfo: Vfo::default(),
            meter: Meter::default(),
            meter_b: Meter::default(),
            err_warn_until: 0.0,
            level_b: None,
            scope: Scope::default(),
            decodes: VecDeque::new(),
            class_log: VecDeque::new(),
            last_class_key: None,
            held: None,
            calls: Vec::new(),
            calls_polled: 0.0,
            devices: Vec::new(),
            toasts: Vec::new(),
            dock_tab: qa_tab,
            decode_filter: String::new(),
            decodes_added: 0,
            decode_scroll: 0.0,
            decodes_unseen: 0,
            decode_jump: false,
            playing: None,
            activity: VecDeque::new(),
            activity_hz: 0.0,
            renaming: None,
            picked: std::sync::mpsc::channel(),
            selected_decode: None,
            scan_form: proto::ScanCfg::default(),
            scan_touched: false,
            p25: Default::default(),
            settings_open: false,
            help_open: false,
            connect_input,
            splash: brand::Splash::default(),
            inspect_pending: None,
            gain_ui: 0.0,
            zoom_ui: 1.0,
            span_pending: None,
            clip_until: 0.0,
            spur: None,
            display_bins_sent: 0,
            display_bins_due: None,
            rate_t: 0.0,
            rate_frames: 0,
            rate_bytes: 0,
            fps: 0.0,
            kbps: 0.0,
            history: VecDeque::new(),
            reconnects: 0,
            connected_at: None,
            diagnostics_open: false,
            replay_note: None,
            recorder_busy: None,
            memory_armed: false,
            audio_devices: None,
            fullscreen: false,
            memories_on_toolbar: false,
            title: String::new(),
        }
    }

    pub fn reduce_motion(&self) -> bool {
        match self.prefs.motion {
            Motion::Full => false,
            Motion::Reduced => true,
            Motion::System => {
                use std::sync::OnceLock;
                static SYS: OnceLock<bool> = OnceLock::new();
                *SYS.get_or_init(crate::prefs::system_reduces_motion)
            }
        }
    }

    pub fn connect(&mut self, ctx: &egui::Context, server: &str) {
        let speakers = Arc::new(crate::audio::Speakers { a: self.player.clone(), b: self.player_b.clone() });
        self.link = Some(Link::open(server, ctx.clone(), speakers));
        self.prefs.remember_server(server);
        self.prefs.save();
        self.conn = Conn::Connecting { attempt: 1, reason: None };
        self.have_status = false;
        self.frame = None;
        self.pan = Panafall::default();
        self.display_bins_sent = 0;
    }

    pub fn disconnect(&mut self) {
        self.link = None;
        self.conn = Conn::Idle;
        self.have_status = false;
        self.frame = None;
        self.player.flush();
    }

    pub fn toast(&mut self, ctx: &egui::Context, tone: Tone, text: impl Into<String>) {
        let until = ctx.input(|i| i.time) + 5.0;
        self.toasts.push(Toast { text: text.into(), tone, until, born: 0.0, height: 0.0 });
        if self.toasts.len() > 4 {
            self.toasts.remove(0);
        }
    }

    pub fn post(&self, path: &str, body: Value) {
        if let Some(l) = &self.link {
            l.post(path, body);
        }
    }

    // ---------- receiver model ----------

    /// The receive frequency the readout shows: the operator's last request
    /// until the server has had a moment to confirm it.
    pub fn inspect_hz(&self) -> f64 {
        match self.inspect_pending {
            Some((hz, at)) if at.elapsed() < Duration::from_millis(900) => hz,
            _ => self.status.inspect_hz,
        }
    }

    pub fn mode_caps(&self) -> Option<&ModeCaps> {
        self.modes.iter().find(|m| m.mode == self.status.mode)
    }

    pub fn step_hz(&self) -> f64 {
        self.prefs
            .step_hz
            .unwrap_or_else(|| crate::tuning::mode_raster_hz(self.status.mode, self.inspect_hz()))
    }

    fn span_now(&self) -> (f64, f64) {
        if self.pan.span_hz > 0.0 {
            (self.pan.center_hz, self.pan.span_hz)
        } else {
            let span = (self.status.max_freq_hz - self.status.min_freq_hz).max(self.status.rate_hz);
            ((self.status.max_freq_hz + self.status.min_freq_hz) / 2.0, span)
        }
    }

    /// Whether this server has a second receiver to offer.
    pub fn rx2_available(&self) -> bool {
        self.server.as_ref().is_some_and(|i| i.has("rx2"))
    }

    /// The frequency the readout and the display's gestures act on.
    pub fn active_hz(&self) -> f64 {
        match (&self.status.rx2, self.active_b) {
            (Some(b), true) => b.freq_hz,
            _ => self.inspect_hz(),
        }
    }

    /// Start or move receiver B. It must stay inside the span; the server
    /// refuses it otherwise and says so.
    pub fn set_rx2(&mut self, hz: f64, mode: Mode) {
        let (lo, hi) = (self.status.min_freq_hz, self.status.max_freq_hz);
        if hi > lo && !(lo..=hi).contains(&hz) {
            self.toasts.push(Toast {
                text: format!("Receiver B listens inside the span ({} – {} MHz)", crate::tuning::fmt_mhz(lo, 3), crate::tuning::fmt_mhz(hi, 3)),
                tone: Tone::Warn,
                until: 0.0,
                born: 0.0,
                height: 0.0,
            });
            return;
        }
        let (blo, bhi) = self.modes.iter().find(|c| c.mode == mode).map(|c| (c.passband_offset_hz as f64 - c.bandwidth_hz as f64 / 2.0, c.passband_offset_hz as f64 + c.bandwidth_hz as f64 / 2.0)).unwrap_or((-6_250.0, 6_250.0));
        let keep = self.status.rx2.as_ref().filter(|b| b.mode == mode).map(|b| (b.bandwidth_hz, b.passband_offset_hz));
        let (bw, off) = keep.unwrap_or((bhi - blo, (blo + bhi) / 2.0));
        self.status.rx2 = Some(proto::SubRx { freq_hz: hz, mode, bandwidth_hz: bw, passband_offset_hz: off, note: None });
        self.post("/api/sdr/rx2", json!({ "freq_hz": hz, "mode": mode.wire() }));
    }

    /// Receiver B at `hz`: started if it is off, moved if it is on. It
    /// listens in A's mode where it can, NFM otherwise.
    pub fn place_b(&mut self, hz: f64) {
        if !self.rx2_available() {
            self.toasts.push(Toast { text: "This server has no second receiver (it predates API 2).".into(), tone: Tone::Warn, until: 0.0, born: 0.0, height: 0.0 });
            return;
        }
        let mode = match &self.status.rx2 {
            Some(b) => b.mode,
            None if matches!(self.status.mode, Mode::Nfm | Mode::Am | Mode::Usb | Mode::Lsb | Mode::Wfm) => self.status.mode,
            None => Mode::Nfm,
        };
        self.set_rx2(hz, mode);
    }

    pub fn rx2_off(&mut self) {
        self.status.rx2 = None;
        self.active_b = false;
        self.player_b.flush();
        self.post("/api/sdr/rx2", json!({ "off": true }));
    }

    pub fn tune(&mut self, hz: f64, lock: bool) {
        if self.active_b
            && let Some(b) = self.status.rx2.clone()
        {
            self.set_rx2(hz, b.mode);
            return;
        }
        let bounds = if self.status.tune_max_hz > self.status.tune_min_hz {
            (self.status.tune_min_hz, self.status.tune_max_hz)
        } else {
            (1e6, 2.2e9)
        };
        // Zoomed in, the window already follows the receiver; the span
        // center is the tuner's, so plan against the full span.
        let (center, span) = if self.status.zoom > 1.01 {
            (self.status.freq_hz, self.status.rate_hz.max(1.0))
        } else {
            self.span_now()
        };
        let locked = self.prefs.span_lock;
        let plan = crate::tuning::plan(hz, center, span, self.status.bandwidth_hz, lock && self.status.zoom <= 1.01, locked, bounds);
        if let Some(c) = plan.center_hz {
            if locked {
                self.toasts.push(Toast {
                    text: format!("Span lock: {} MHz is outside the span, so the span moved to reach it.", crate::tuning::fmt_mhz(plan.inspect_hz, 4)),
                    tone: Tone::Idle,
                    until: 0.0,
                    born: 0.0,
                    height: 0.0,
                });
            }
            self.post("/api/sdr/tune", json!({ "freq_hz": c }));
        }
        self.post("/api/sdr/inspect", json!({ "freq_hz": plan.inspect_hz }));
        self.inspect_pending = Some((plan.inspect_hz, Instant::now()));
    }

    pub fn center_span(&mut self, hz: f64) {
        self.post("/api/sdr/tune", json!({ "freq_hz": hz }));
    }

    pub fn set_mode(&mut self, m: Mode) {
        if m != Mode::Scan {
            self.prefs.last_voice_mode = m;
        }
        self.status.mode = m;
        self.post("/api/sdr/mode", json!({ "mode": m.wire() }));
    }

    /// The filter the current mode lets the operator set, when the server
    /// can take one.
    pub fn filter_range(&self) -> Option<proto::FilterRange> {
        if !self.server.as_ref().is_some_and(|i| i.has("filter")) {
            return None;
        }
        self.mode_caps().and_then(|c| c.filter)
    }

    pub fn set_filter(&mut self, lo: f64, hi: f64) {
        // Shown at once; the server's status confirms or corrects it.
        self.status.filter_low_hz = lo;
        self.status.filter_high_hz = hi;
        self.status.bandwidth_hz = hi - lo;
        self.status.passband_offset_hz = (lo + hi) / 2.0;
        self.status.filter_custom = true;
        self.post("/api/sdr/filter", json!({ "low_hz": lo, "high_hz": hi }));
    }

    pub fn reset_filter(&mut self) {
        self.post("/api/sdr/filter", json!({ "reset": true }));
    }

    pub fn set_zoom(&mut self, z: f64) {
        let z = if z < 1.04 { 1.0 } else { z.min(32.0) };
        self.zoom_ui = z;
        self.post("/api/sdr/zoom", json!({ "zoom": z }));
    }

    pub fn memories(&self) -> Vec<Memory> {
        self.memories.clone().unwrap_or_else(crate::prefs::default_memories)
    }

    pub fn save_memories(&mut self, list: Vec<Memory>) {
        self.memories = Some(list.clone());
        self.post("/api/memories", json!({ "memories": list }));
    }

    fn push_decode(&mut self, row: DecodeRow) {
        self.decodes.push_front(row);
        self.decodes_added += 1;
        if self.decode_scroll > 1.0 {
            self.decodes_unseen += 1;
        }
        self.decodes.truncate(400);
        self.selected_decode = self.selected_decode.map(|i| i + 1).filter(|&i| i < self.decodes.len());
    }

    fn handle(&mut self, ctx: &egui::Context, ev: NetEvent) {
        let now = ctx.input(|i| i.time);
        match ev {
            NetEvent::Connecting { attempt } => {
                if !matches!(self.conn, Conn::Live) {
                    let reason = match &self.conn {
                        Conn::Connecting { reason, .. } => reason.clone(),
                        _ => None,
                    };
                    self.conn = Conn::Connecting { attempt, reason };
                }
            }
            NetEvent::Connected(s, info) => {
                if self.connected_at.is_some() {
                    self.reconnects += 1;
                }
                self.connected_at = Some(Instant::now());
                self.conn = Conn::Live;
                let old = info.as_ref().is_none_or(|i| i.api < proto::API_REVISION);
                if old && self.server.as_ref().map(|i| i.api) != info.as_ref().map(|i| i.api) {
                    self.toast(ctx, Tone::Warn, "This server is older than the app: presence, the web page setting, shared memories and the filter are unavailable until it is updated.");
                }
                self.server = info;
                self.apply_status(*s);
                self.display_bins_sent = 0;
                if !self.prefs.hinted_shortcuts {
                    self.prefs.hinted_shortcuts = true;
                    self.prefs.save();
                    self.toast(ctx, Tone::Idle, "Press ? for the shortcuts and gestures: click, wheel and drag the display; arrows and PgUp/PgDn tune.");
                }
                if let Some(l) = &self.link {
                    l.get("/api/sdr/modes", Get::Modes);
                    l.get("/api/devices", Get::Devices);
                    l.get("/api/sdr/calls", Get::Calls);
                    l.get("/api/memories", Get::Memories);
                }
                self.player.flush();
            }
            NetEvent::Disconnected(reason) => {
                let was_live = matches!(self.conn, Conn::Live);
                self.conn = Conn::Connecting { attempt: 0, reason: Some(reason.clone()) };
                if was_live {
                    self.toast(ctx, Tone::Warn, "Connection to the server lost. Reconnecting.");
                }
            }
            NetEvent::Fft(f) => {
                self.pan.push(&f, &self.prefs.view, now);
                self.meter.feed(f.channel_dbfs, f.noise_dbfs, f.peak_iq);
                self.level_b = self.status.rx2.as_ref().and_then(|b| proto::channel_level(&f.pwr, f.center_hz, f.rate_hz, b.freq_hz + b.passband_offset_hz, b.bandwidth_hz));
                if let Some((ch, nz)) = self.level_b {
                    self.meter_b.feed(ch, nz, f.peak_iq);
                }
                if f.peak_iq >= 0.98 {
                    self.clip_until = now + 2.5;
                }
                if (f.inspect_hz - self.activity_hz).abs() >= 1.0 {
                    self.activity.clear();
                    self.activity_hz = f.inspect_hz;
                }
                self.activity.push_back((now, f.channel_dbfs, f.noise_dbfs));
                while self.activity.front().is_some_and(|a| a.0 < now - 20.0) {
                    self.activity.pop_front();
                }
                self.classify(&f);
                self.spur_step(&f, now);
                self.frame = Some(f);
            }
            NetEvent::Status(s) => self.apply_status(*s),
            NetEvent::Decode { inspect_hz, event, at } => {
                let call = event.kind == "call";
                self.push_decode(DecodeRow::new(at, inspect_hz, event));
                if call && let Some(l) = &self.link {
                    l.get("/api/sdr/calls", Get::Calls);
                }
            }
            NetEvent::Presence(p) => {
                let before = self.presence.web_count();
                if p.web_count() > before && self.have_status {
                    self.toast(ctx, Tone::Idle, "A web page joined this receiver.");
                }
                self.presence = p;
            }
            NetEvent::Reply { what, result } => self.reply(ctx, what, result),
            NetEvent::Saved { path, bytes } => {
                self.recorder_busy = None;
                self.toast(ctx, Tone::Ok, format!("Saved {} · {:.1} MiB", path.display(), bytes as f64 / 1_048_576.0));
            }
            NetEvent::Bytes { what, bytes, headers } => self.bytes(ctx, what, bytes, headers),
            NetEvent::Failed { what, error } => {
                self.recorder_busy = None;
                let msg = if error.trim().is_empty() { "request failed".to_string() } else { error };
                self.toast(ctx, Tone::Err, format!("{what}: {msg}"));
            }
            NetEvent::Refused(msg) => self.toast(ctx, Tone::Warn, msg),
        }
    }

    fn apply_status(&mut self, s: Status) {
        if !self.have_status || (self.gain_ui - s.gain_db.unwrap_or(self.gain_ui)).abs() > 0.01 {
            if let Some(g) = s.gain_db {
                self.gain_ui = g;
            } else if let Some(g) = s.gain_now_db {
                self.gain_ui = g;
            }
        }
        self.zoom_ui = s.zoom.max(1.0);
        if let Some((hz, _)) = self.inspect_pending
            && (hz - s.inspect_hz).abs() < 1.0
        {
            self.inspect_pending = None;
        }
        if let Some(scan) = &s.scan
            && !self.scan_touched
        {
            self.scan_form = scan.cfg.clone();
        }
        if let Some((want, _)) = self.span_pending
            && (want - s.rate_hz).abs() < 1.0
        {
            self.span_pending = None;
        }
        if s.rx2.is_none() {
            self.active_b = false;
        }
        self.status = s;
        self.have_status = true;
    }

    fn reply(&mut self, ctx: &egui::Context, what: Get, result: Result<Value, String>) {
        match (what, result) {
            (Get::Modes, Ok(v)) => {
                if let Ok(m) = serde_json::from_value::<Vec<ModeCaps>>(v) {
                    self.modes = m;
                }
            }
            (Get::Devices, Ok(v)) => {
                if let Ok(d) = serde_json::from_value::<Vec<Device>>(v) {
                    self.devices = d;
                }
            }
            (Get::Calls, Ok(v)) => {
                if let Ok(c) = serde_json::from_value::<Vec<Call>>(v) {
                    self.calls = c;
                }
            }
            (Get::Memories, Ok(v)) => {
                // Older servers have no bank and answer 404: keep the defaults.
                if let Some(list) = v.get("memories").filter(|m| !m.is_null())
                    && let Ok(m) = serde_json::from_value::<Vec<Memory>>(list.clone()) {
                        self.memories = Some(m);
                    }
            }
            (Get::Memories, Err(_)) => {}
            (Get::P25Key, Ok(v)) => self.p25.load(v),
            (Get::P25Key, Err(e)) => self.p25.message = Some((Tone::Err, e)),
            (Get::Calibrate, Ok(v)) => {
                let ppm = v.get("ppm").and_then(Value::as_f64).unwrap_or(0.0);
                let err = v.get("measuredErrorHz").and_then(Value::as_f64).unwrap_or(0.0);
                let r = v.get("referenceHz").and_then(Value::as_f64).unwrap_or(0.0);
                self.toast(ctx, Tone::Ok, format!("Calibrated to {ppm:.1} ppm from {} MHz ({err:+.0} Hz error)", crate::tuning::fmt_mhz(r, 5)));
            }
            (Get::Calibrate, Err(e)) => self.toast(ctx, Tone::Err, format!("Calibrate: {e}")),
            (Get::Replay, Ok(v)) => {
                let hz = v.get("frequencyHz").and_then(Value::as_f64).unwrap_or(self.inspect_hz());
                let events = v.get("events").and_then(Value::as_array).cloned().unwrap_or_default();
                for e in &events {
                    if let Ok(ev) = serde_json::from_value::<proto::DecodeEvent>(e.clone()) {
                        self.push_decode(DecodeRow::new(SystemTime::now(), hz, ev));
                    }
                }
                let proto_name = v
                    .pointer("/classification/protocol")
                    .and_then(Value::as_str)
                    .unwrap_or("no match")
                    .to_string();
                let dur = v.get("durationS").and_then(Value::as_f64).unwrap_or(0.0);
                self.replay_note = Some(format!("Replay: {dur:.1} s · {} events · {proto_name}", events.len()));
                self.recorder_busy = None;
                self.dock_tab = DockTab::Decodes;
            }
            (Get::Replay, Err(e)) => {
                self.recorder_busy = None;
                self.toast(ctx, Tone::Err, format!("Replay: {e}"));
            }
            // Background refreshes: the connection state already says why.
            (Get::Calls | Get::Devices | Get::Modes, Err(_)) => {}
        }
    }

    fn bytes(&mut self, ctx: &egui::Context, what: Fetch, bytes: Vec<u8>, headers: Vec<(String, String)>) {
        let header = |k: &str| headers.iter().find(|(h, _)| h == k).map(|(_, v)| v.clone());
        match what {
            Fetch::PlayCapture | Fetch::CallAudio(_) => {
                self.recorder_busy = None;
                match crate::wav::parse(&bytes) {
                    Some(w) if !w.samples.is_empty() => {
                        self.player.play_clip(w.mono(), w.rate);
                        self.playing = Some(if let Fetch::CallAudio(id) = what { Playing::Call(id) } else { Playing::Capture });
                    }
                    _ => {
                        self.playing = None;
                        self.toast(ctx, Tone::Warn, "Nothing to play: the audio buffer is empty on this channel.");
                    }
                }
            }
            Fetch::ReplayCapture => {
                let hz = header("x-usdr-frequency-hz").and_then(|v| v.parse().ok()).unwrap_or(self.inspect_hz());
                if let Some(l) = &self.link {
                    l.upload(&format!("/api/sdr/replay?frequency_hz={hz}"), bytes, Get::Replay);
                }
            }
            Fetch::SaveCapture { kind, stem } => {
                self.recorder_busy = None;
                let Some(w) = crate::wav::parse(&bytes) else {
                    self.toast(ctx, Tone::Err, "The capture was not a WAV file.");
                    return;
                };
                if w.samples.is_empty() {
                    self.toast(ctx, Tone::Warn, "The buffer is empty — the channel was just retuned.");
                    return;
                }
                let freq = header("x-usdr-frequency-hz").and_then(|v| v.parse::<f64>().ok()).unwrap_or(self.inspect_hz());
                let result = crate::wav::save_capture(&kind, &stem, &w, freq, &self.status);
                match result {
                    Ok(path) => {
                        let secs = w.frames() as f64 / w.rate.max(1) as f64;
                        self.toast(ctx, Tone::Ok, format!("Saved {} · {secs:.1} s", path.display()));
                    }
                    Err(e) => self.toast(ctx, Tone::Err, format!("Saving capture: {e}")),
                }
            }
        }
    }

    /// The classification log, as the web page keeps it: a row when the
    /// protocol or channel changes or three seconds pass, and the last real
    /// detection held for 3.5 s after the signal drops.
    fn classify(&mut self, f: &FftFrame) {
        let Some(c) = &f.classification else { return };
        let real = !matches!(c.protocol.as_str(), "Idle / Noise" | "Noise" | "FLEX Search") && !c.protocol.is_empty();
        if self.status.mode == Mode::Flex && !c.protocol.contains("FLEX") {
            return;
        }
        if real && c.active {
            self.held = Some((c.clone(), Instant::now()));
            let key = format!("{:.4}_{}", f.inspect_hz / 1e6, c.protocol);
            let due = match &self.last_class_key {
                Some((k, at)) => *k != key || at.elapsed() > Duration::from_secs(3),
                None => true,
            };
            if due {
                self.last_class_key = Some((key, Instant::now()));
                self.class_log.push_front(ClassRow { at: SystemTime::now(), freq_hz: f.inspect_hz, c: c.clone() });
                self.class_log.truncate(100);
            }
        }
    }

    /// What the receiver is hearing: the live classification, or the last
    /// real one while it is held.
    /// RDS from the station receiver A is on, when the server has a
    /// confirmed PI for it.
    /// With A hard left and B hard right (the L|R split), each receiver
    /// plays as mono, so a stereo station's right channel is not lost from
    /// A's ear. Balance alone, anywhere else, keeps stereo.
    pub fn sync_fold(&self) {
        let split = self.prefs.pan <= -0.99 && self.prefs.pan_b >= 0.99;
        self.player.set_fold(split);
        self.player_b.set_fold(split);
    }

    pub fn rds(&self) -> Option<&usdr_proto::Rds> {
        self.status.rds.as_ref().filter(|_| self.status.mode == Mode::Wfm)
    }

    pub fn shown_class(&self) -> (Option<Classification>, &'static str) {
        let live = self.frame.as_ref().and_then(|f| f.classification.clone());
        if let Some(c) = &live
            && c.active
        {
            return (live, "ACTIVE");
        }
        if let Some((c, at)) = &self.held
            && at.elapsed() < Duration::from_millis(3500)
        {
            return (Some(c.clone()), "HELD");
        }
        (live, "IDLE")
    }

    pub fn start_spur_check(&mut self, now: f64) {
        if !self.status.spurs_hz.is_empty() {
            self.post("/api/sdr/spurs", json!({ "offsets_hz": [] }));
            return;
        }
        self.spur = Some(SpurCheck::Settle { until: now + 0.7, home: self.status.freq_hz });
    }

    fn spur_step(&mut self, f: &FftFrame, now: f64) {
        let rel = |p: &[f32]| {
            let mut s = p.to_vec();
            s.sort_by(|a, b| a.total_cmp(b));
            let med = s[s.len() / 2];
            p.iter().map(|v| v - med).collect::<Vec<f32>>()
        };
        let Some(state) = self.spur.take() else { return };
        self.spur = match state {
            SpurCheck::Settle { until, home } if now >= until && f.pwr.len() > 4 => {
                let a = rel(&f.pwr);
                self.center_span(home + f.rate_hz / 8.0);
                Some(SpurCheck::Shifted { until: now + 1.4, home, a })
            }
            SpurCheck::Shifted { until, home, a } if now >= until => {
                let b = rel(&f.pwr);
                self.center_span(home);
                Some(SpurCheck::Back { until: now + 0.9, a, b, rate: f.rate_hz })
            }
            SpurCheck::Back { until, a, b, rate } if now >= until => {
                // A spur moves with the tuner, so it sits in the same bin in
                // both frames; a real signal moved by an eighth of the span.
                let n = a.len().min(b.len());
                let mut offs: Vec<f64> = Vec::new();
                for i in 2..n.saturating_sub(2) {
                    if a[i] >= 4.0 && b[i] >= 4.0 {
                        let off = (i as f64 / (n - 1) as f64 - 0.5) * rate;
                        if offs.last().is_none_or(|l| off - l > 4.0 * rate / n as f64) {
                            offs.push(off);
                        }
                    }
                }
                offs.truncate(32);
                self.post("/api/sdr/spurs", json!({ "offsets_hz": offs }));
                None
            }
            other => Some(other),
        };
    }

    fn rates(&mut self, now: f64) {
        let Some(l) = &self.link else { return };
        if now - self.rate_t >= 1.0 {
            let frames = l.stats.fft_frames.load(Ordering::Relaxed);
            let bytes = l.stats.bytes.load(Ordering::Relaxed);
            let dt = (now - self.rate_t) as f32;
            if self.rate_t > 0.0 {
                self.fps = (frames - self.rate_frames) as f32 / dt;
                self.kbps = (bytes - self.rate_bytes) as f32 / dt / 1024.0;
            }
            self.rate_t = now;
            self.rate_frames = frames;
            self.rate_bytes = bytes;
            let rtt = l.stats.rtt_us.load(Ordering::Relaxed) as f32 / 1000.0;
            self.history.push_back((self.fps, self.kbps, rtt));
            while self.history.len() > 120 {
                self.history.pop_front();
            }
        }
    }

    // ---------- keyboard ----------

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() || self.vfo.editing() {
            return;
        }
        let (keys, mods, text) = ctx.input(|i| {
            let keys: Vec<egui::Key> = i
                .events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Key { key, pressed: true, .. } => Some(*key),
                    _ => None,
                })
                .collect();
            let text: String = i
                .events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Text(t) => Some(t.clone()),
                    _ => None,
                })
                .collect();
            (keys, i.modifiers, text)
        });
        use egui::Key;
        for k in keys {
            match k {
                Key::F11 => {
                    self.fullscreen = !self.fullscreen;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
                }
                Key::Comma if mods.command => self.settings_open = !self.settings_open,
                _ if !self.prefs.shortcuts || !self.have_status => {}
                Key::L if !mods.command => self.prefs.span_lock = !self.prefs.span_lock,
                Key::H if !mods.command => self.pan.toggle_hold(),
                Key::B if !mods.command => self.active_b = !self.active_b && self.status.rx2.is_some(),
                Key::ArrowUp | Key::ArrowDown => {
                    let d = if k == Key::ArrowUp { 1.0 } else { -1.0 };
                    let hz = self.active_hz() + self.vfo.step_selected(d);
                    self.tune(hz, true);
                }
                Key::ArrowLeft => self.vfo.move_selection(-1),
                Key::ArrowRight => self.vfo.move_selection(1),
                Key::PageUp | Key::PageDown => {
                    let d = if k == Key::PageUp { 1.0 } else { -1.0 } * if mods.shift { 10.0 } else { 1.0 };
                    let step = self.step_hz();
                    let hz = crate::tuning::snap(self.active_hz() + d * step, step);
                    self.tune(hz, true);
                }
                // M and R act on the receiver being driven, as the bar's keys do.
                Key::M if !mods.command => {
                    if self.active_b && self.status.rx2.is_some() {
                        self.prefs.muted_b = !self.prefs.muted_b;
                        self.player_b.set_muted(self.prefs.muted_b);
                    } else {
                        self.prefs.muted = !self.prefs.muted;
                        self.player.set_muted(self.prefs.muted);
                    }
                }
                Key::Plus | Key::Equals => self.set_zoom(self.zoom_ui * 2.0),
                Key::Minus => self.set_zoom(self.zoom_ui / 2.0),
                Key::C if !mods.command => self.center_span(self.inspect_hz()),
                Key::D if !mods.command => self.prefs.show_dock = !self.prefs.show_dock,
                Key::S if !mods.command => self.prefs.show_side = !self.prefs.show_side,
                Key::R if !mods.command => {
                    if self.active_b && self.status.rx2.is_some() {
                        self.toggle_recording_b(ctx);
                    } else {
                        self.toggle_local_recording(ctx);
                    }
                }
                Key::Space => self.vfo.open_entry(self.inspect_hz(), Some(String::new())),
                Key::Num1 | Key::Num2 | Key::Num3 | Key::Num4 | Key::Num5 | Key::Num6 | Key::Num7 | Key::Num8 | Key::Num9 if mods.command => {
                    let idx = k.name().parse::<usize>().unwrap_or(1) - 1;
                    if let Some(m) = self.memories().get(idx).cloned() {
                        self.tune(m.freq_hz, true);
                        self.set_mode(m.mode);
                    }
                }
                _ => {}
            }
        }
        if !self.prefs.shortcuts || mods.command {
            return;
        }
        if text == "?" {
            self.help_open = !self.help_open;
        } else if let Some(c) = text.chars().next().filter(|c| c.is_ascii_digit() || *c == '.') {
            // Typing a number starts frequency entry with it.
            self.vfo.open_entry(self.inspect_hz(), Some(c.to_string()));
        }
    }

    /// Record receiver B's audio, as R does A's.
    pub fn toggle_recording_b(&mut self, ctx: &egui::Context) {
        if let Some((path, secs)) = self.player_b.stop_recording() {
            self.toast(ctx, Tone::Ok, format!("Recorded {secs:.1} s of receiver B to {}", path.display()));
            return;
        }
        let Some(b) = self.status.rx2.clone() else { return };
        let path = self.prefs.record_dir().join(format!(
            "usdr-audio-B-{}-{}MHz-{}.wav",
            crate::wav::stamp(),
            crate::tuning::fmt_mhz(b.freq_hz, 5),
            b.mode.wire()
        ));
        if let Err(e) = self.player_b.start_recording(path) {
            self.toast(ctx, Tone::Err, format!("Could not start recording: {e}"));
        }
    }

    pub fn toggle_local_recording(&mut self, ctx: &egui::Context) {
        if let Some((path, secs)) = self.player.stop_recording() {
            self.toast(ctx, Tone::Ok, format!("Recorded {secs:.1} s to {}", path.display()));
            return;
        }
        let stamp = crate::wav::stamp();
        let path = self.prefs.record_dir().join(format!(
            "usdr-audio-{stamp}-{}MHz-{}.wav",
            crate::tuning::fmt_mhz(self.inspect_hz(), 5),
            self.status.mode.wire()
        ));
        if let Err(e) = self.player.start_recording(path) {
            self.toast(ctx, Tone::Err, format!("Could not start recording: {e}"));
        }
    }

    fn display_bins(&mut self, ctx: &egui::Context, width_px: f32, now: f64) {
        let bins = (width_px * ctx.pixels_per_point()).round() as usize;
        if bins < 64 || bins == self.display_bins_sent {
            self.display_bins_due = None;
            return;
        }
        match self.display_bins_due {
            Some((b, at)) if b == bins && now >= at => {
                self.post("/api/sdr/display", json!({ "bins": bins.min(16384) }));
                self.display_bins_sent = bins;
                self.display_bins_due = None;
            }
            Some((b, _)) if b == bins => {}
            _ => self.display_bins_due = Some((bins, now + 0.4)),
        }
        if self.display_bins_due.is_some() {
            ctx.request_repaint_after(Duration::from_millis(450));
        }
    }

    // ---------- layout ----------

    fn masthead(&mut self, ui: &mut Ui) {
        let now = ui.input(|i| i.time);
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            let (r, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
            brand::mark_static(ui.painter(), r);
            theme::steel_label(ui, "uSDR", theme::wordmark(20.0));
            ui.add_space(6.0);
            let s = &self.status;
            if self.have_status {
                ui.label(egui::RichText::new(s.radio_label()).font(theme::mono(11.5)).color(theme::text_2()));
                sep(ui);
                // The radio's own state only: span, channel width, span lock and
                // the protocol heard all show in the receiver bar and toolbar.
                annunciator(ui, "RATE", &crate::tuning::fmt_short_hz(s.rate_hz), theme::text());
                if s.lo_offset {
                    annunciator(ui, "LO", &format!("{:+}", crate::tuning::fmt_short_hz(s.lo_offset_hz)), theme::CYAN);
                }
                sep(ui);
                let streaming = self.pan.last_frame.is_some_and(|t| t.elapsed() < Duration::from_secs(2));
                if !streaming {
                    widgets::chip(ui, Tone::Idle, "ADC —");
                } else if now < self.clip_until {
                    widgets::chip(ui, Tone::Err, "ADC CLIP");
                } else {
                    widgets::chip(ui, Tone::Ok, "ADC OK");
                }
                let drops = s.dropped_blocks + s.lagged_blocks;
                if let Some(e) = &s.error {
                    widgets::chip(ui, Tone::Err, "NO RADIO").on_hover_text(e.clone());
                } else if !streaming {
                    widgets::chip(ui, Tone::Idle, "STREAM —");
                } else if drops > 0 || s.lost_samples > 0 {
                    widgets::chip(ui, Tone::Warn, &format!("STREAM {drops} drop · {} gap", s.gap_events))
                        .on_hover_text(format!("{} lost samples since start", s.lost_samples));
                } else {
                    widgets::chip(ui, Tone::Ok, "STREAM OK");
                }
                let rec = match (self.player.reading().recording.is_some(), self.player_b.reading().recording.is_some()) {
                    (true, true) => Some("REC A B"),
                    (true, false) => Some("REC"),
                    (false, true) => Some("REC B"),
                    _ => None,
                };
                if let Some(t) = rec {
                    widgets::chip(ui, Tone::Live, t);
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                if icon_button(ui, "⚙", "Settings (Ctrl+,)").clicked() {
                    self.settings_open = !self.settings_open;
                }
                if icon_button(ui, "?", "Shortcuts and gestures (?)").clicked() {
                    self.help_open = !self.help_open;
                }
                self.view_menu(ui);
                let utc = utc_hms();
                ui.label(egui::RichText::new(format!("{utc} UTC")).font(theme::mono(12.0)).color(theme::text_2()));
                self.connection_chip(ui);
                self.presence_badge(ui);
            });
        });
        ui.ctx().request_repaint_after(Duration::from_millis(500));
    }

    fn connection_chip(&mut self, ui: &mut Ui) {
        let (tone, text) = match &self.conn {
            Conn::Live => (Tone::Ok, self.link.as_ref().map(|l| l.base.trim_start_matches("http://").to_string()).unwrap_or_default()),
            Conn::Connecting { attempt, reason } => (
                Tone::Warn,
                match reason {
                    Some(r) if *attempt > 0 => format!("reconnecting · {r}"),
                    Some(r) => r.clone(),
                    None => "connecting".into(),
                },
            ),
            Conn::Idle => (Tone::Idle, "not connected".into()),
        };
        let text: String = text.chars().take(48).collect();
        let resp = widgets::chip(ui, tone, &text);
        // Signal bars: frame rate against the 25 fps the server sends.
        if let Conn::Live = self.conn {
            let (r, _) = ui.allocate_exact_size(vec2(18.0, 14.0), Sense::hover());
            let bars = ((self.fps / 25.0) * 4.0).round().clamp(0.0, 4.0) as usize;
            for i in 0..4 {
                let h = 4.0 + i as f32 * 3.0;
                let b = Rect::from_min_max(pos2(r.left() + i as f32 * 4.5, r.bottom() - h), pos2(r.left() + i as f32 * 4.5 + 3.0, r.bottom()));
                ui.painter().rect_filled(b, CornerRadius::same(1), if i < bars { theme::OK } else { theme::surface_4() });
            }
        }
        if resp.interact(Sense::click()).clicked() {
            self.diagnostics_open = !self.diagnostics_open;
        }
        resp.on_hover_text(match &self.link {
            Some(l) => format!(
                "{} · {:.0} fps · {:.0} KiB/s · API {:.1} ms",
                l.base,
                self.fps,
                self.kbps,
                l.stats.rtt_us.load(Ordering::Relaxed) as f32 / 1000.0
            ),
            None => "Not connected".into(),
        });
    }

    fn view_menu(&mut self, ui: &mut Ui) {
        widgets::menu_chip(ui, "VIEW", theme::text_2(), |ui| {
            ui.set_min_width(220.0);
            ui.checkbox(&mut self.prefs.show_side, "Radio and tools column   S");
            ui.checkbox(&mut self.prefs.show_dock, "Dock   D");
            ui.checkbox(&mut self.prefs.show_scope, "Scope in the dock");
            ui.checkbox(&mut self.prefs.show_memories, "Memory row");
            ui.checkbox(&mut self.prefs.view.band_plan, "Band plan");
            ui.checkbox(&mut self.prefs.span_lock, "Span lock   L");
            ui.separator();
            if ui.button("Compact: display only").clicked() {
                self.prefs.show_side = false;
                self.prefs.show_dock = false;
                ui.close();
            }
            if ui.button("Full layout").clicked() {
                self.prefs.show_side = true;
                self.prefs.show_dock = true;
                ui.close();
            }
            if ui.button(if self.fullscreen { "Leave full screen   F11" } else { "Full screen   F11" }).clicked() {
                self.fullscreen = !self.fullscreen;
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
                ui.close();
            }
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("Text size");
                let z = ui.ctx().zoom_factor();
                if ui.small_button("−").clicked() {
                    ui.ctx().set_zoom_factor((z - 0.1).max(0.6));
                }
                ui.label(egui::RichText::new(format!("{:.0}%", z * 100.0)).font(theme::mono(11.0)));
                if ui.small_button("+").clicked() {
                    ui.ctx().set_zoom_factor((z + 0.1).min(2.0));
                }
                if ui.small_button("reset").clicked() {
                    ui.ctx().set_zoom_factor(1.0);
                }
            });
            ui.horizontal(|ui| {
                ui.label("Frame rate");
                for fps in [60u32, 30] {
                    if ui.radio(self.prefs.max_fps == fps, format!("{fps}")).clicked() {
                        self.prefs.max_fps = fps;
                        crate::anim::set_max_fps(fps);
                    }
                }
            });
            ui.separator();
            if ui.button("Connection diagnostics…").clicked() {
                self.diagnostics_open = true;
                ui.close();
            }
        });
    }

    fn presence_badge(&mut self, ui: &mut Ui) {
        let others: Vec<&proto::ClientInfo> = self
            .presence
            .clients
            .iter()
            .filter(|c| !(c.kind == "desktop" && c.name == crate::net::hostname()))
            .collect();
        if others.is_empty() && self.presence.web == proto::WebPolicy::Shared {
            return;
        }
        let web = self.presence.web_count();
        let text = match self.presence.web {
            proto::WebPolicy::Off => "WEB OFF".to_string(),
            proto::WebPolicy::Yield => format!("WEB VIEW-ONLY · {web}"),
            proto::WebPolicy::Shared => format!("SHARED · {}", others.len()),
        };
        let resp = widgets::chip(ui, Tone::Idle, &text);
        resp.on_hover_ui(|ui| {
            ui.label(egui::RichText::new("Connected to this receiver").font(theme::sans_semibold(12.0)));
            for c in &self.presence.clients {
                ui.label(egui::RichText::new(format!("{} · {} · {}", c.kind, if c.name.is_empty() { "—" } else { &c.name }, c.addr)).font(theme::mono(11.0)).color(theme::text_2()));
            }
            ui.label(egui::RichText::new(format!("Web page: {}", self.presence.web.label())).color(theme::text_3()));
        });
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 14.0;
            let small = |ui: &mut Ui, t: String, c| {
                ui.label(egui::RichText::new(t).font(theme::mono(10.5)).color(c));
            };
            // Each number is padded to a fixed width in the mono face, so a
            // change in its digit count (the audio queue crosses 100 ms many
            // times a second) moves nothing after it, its own unit included.
            if let Some(l) = &self.link {
                small(ui, format!("{:>3.0} fps", self.fps), theme::text_2());
                small(ui, format!("{:>5.0} KiB/s", self.kbps), theme::text_3());
                small(ui, format!("API {:>5.1} ms", l.stats.rtt_us.load(Ordering::Relaxed) as f32 / 1000.0), theme::text_3());
            }
            let a = self.player.reading();
            if a.queue_ms > 0.0 {
                small(ui, format!("audio {:>4.0} ms", a.queue_ms), theme::text_3());
            } else {
                small(ui, format!("audio {:>4} ms", "—"), theme::text_3());
            }
            if a.underruns > 0 {
                small(ui, format!("{:>5} underruns", a.underruns), theme::WARN);
            }
            // Errors as chips: the gist, the whole of it on hover, so one
            // long driver message cannot push the rest off the bar.
            if self.have_status
                && let Some(e) = self.status.error.clone()
            {
                error_chip(ui, "SERVER", &e, false);
            }
            let audio_err = self.player.error.lock().unwrap().clone();
            if let Some(e) = audio_err
                && error_chip(ui, "AUDIO", &e, true).clicked()
            {
                let dev = self.prefs.audio_device.clone();
                self.player.open(dev.as_deref());
                self.player_b.open(dev.as_deref());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                brand::credit(ui);
                if let Some((path, secs)) = &a.recording {
                    small(ui, format!("● recording {secs:.1} s → {}", path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default()), theme::EMBER);
                }
            });
        });
    }

    fn toasts_ui(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        let rm = self.reduce_motion();
        for t in &mut self.toasts {
            if t.until == 0.0 {
                t.until = now + 5.0;
            }
            if t.born == 0.0 {
                t.born = now;
            }
        }
        self.toasts.retain(|t| t.until > now);
        if self.toasts.is_empty() {
            return;
        }
        // Each toast is its own area, stacked up from the corner, so a new
        // one can rise into place while the others hold still.
        let mut stack = 0.0;
        for (i, t) in self.toasts.iter_mut().enumerate() {
            let fade_out = ((t.until - now) as f32 / 0.4).clamp(0.0, 1.0);
            let born = if rm { 1.0 } else { ((now - t.born) as f32 / 0.18).clamp(0.0, 1.0) };
            let rise = 1.0 - (1.0 - born).powi(3);
            let fade = fade_out.min(rise);
            let r = egui::Area::new(egui::Id::new(("toast", i)))
                .anchor(Align2::RIGHT_BOTTOM, vec2(-16.0, -40.0 - stack - 8.0 * (1.0 - rise)))
                .order(egui::Order::Foreground)
                .interactable(false)
                .show(ctx, |ui| {
                    egui::Frame::new()
                        .fill(alpha(theme::surface_3(), 0.97 * fade))
                        .stroke(Stroke::new(1.0, alpha(theme::line_2(), fade)))
                        .corner_radius(CornerRadius::same(5))
                        .inner_margin(egui::Margin::symmetric(12, 8))
                        .shadow(egui::Shadow { offset: [0, 6], blur: 18, spread: 0, color: egui::Color32::from_black_alpha((120.0 * fade) as u8) })
                        .show(ui, |ui| {
                            ui.set_max_width(460.0);
                            ui.horizontal(|ui| {
                                widgets::chip(ui, t.tone, match t.tone {
                                    Tone::Ok => "OK",
                                    Tone::Warn => "NOTE",
                                    Tone::Err => "ERROR",
                                    Tone::Live => "LIVE",
                                    Tone::Idle => "INFO",
                                });
                                ui.label(egui::RichText::new(&t.text).color(alpha(theme::text(), fade)));
                            });
                        });
                });
            t.height = r.response.rect.height();
            stack += t.height + 6.0;
        }
        crate::anim::more(ctx);
    }

    fn panafall(&mut self, ui: &mut Ui) {
        let mut acts = Vec::new();
        let rec = self.player.reading().recording.is_some();
        let flag = SliceFlag {
            freq_hz: self.inspect_hz(),
            mode: self.status.mode,
            bandwidth_hz: self.status.bandwidth_hz,
            meter: self.meter.rf.map(crate::meter::frac).unwrap_or(0.0),
            s_label: crate::meter::s_label(self.meter.rf),
            snr_db: self.meter.snr,
            // A broadcast station's own name, once RDS has confirmed it; for
            // one that scrolls words through its PS, the title it tags.
            protocol: self.shown_class().0.filter(|c| c.active).map(|c| c.protocol),
            rds: self.rds().map(|r| crate::spectrum::FlagRds {
                station: crate::bar::rds_station(r),
                kind: r.pty_name.clone().filter(|_| r.pty != Some(0)),
                now: crate::bar::rds_now(r),
                locked: r.locked,
            }),
            recording: rec,
        };
        let step = self.step_hz();
        let rm = self.reduce_motion();
        let width = ui.available_width() - 44.0;
        let now = ui.input(|i| i.time);
        self.pan.filter = self.filter_range().filter(|_| !self.active_b).map(|r| (r, self.status.filter_low_hz as f32, self.status.filter_high_hz as f32));
        self.pan.rx_b = self.status.rx2.as_ref().map(|b| crate::spectrum::RxB {
            freq_hz: b.freq_hz,
            mode: b.mode,
            bandwidth_hz: b.bandwidth_hz,
            offset_hz: b.passband_offset_hz,
            active: self.active_b,
            level: self.level_b.map(|l| l.0),
            note: b.note.clone(),
        });
        self.display_bins(&ui.ctx().clone(), width, now);
        self.pan.ui(ui, &self.status, self.frame.as_deref(), &mut self.prefs.view, &flag, step, rm, &mut acts);
        for a in acts {
            match a {
                Act::Tune { hz, lock } => self.tune(hz, lock),
                Act::Center(hz) => self.center_span(hz),
                Act::Zoom(z) => self.set_zoom(z),
                Act::StoreMemory(hz) => {
                    let mut list = self.memories();
                    list.push(Memory { name: self.status.mode.label().into(), freq_hz: hz, mode: self.status.mode });
                    self.save_memories(list);
                }
                Act::CopyFreq(hz) => ui.ctx().copy_text(format!("{:.0}", hz)),
                Act::Filter(lo, hi) => self.set_filter(lo as f64, hi as f64),
                Act::Select(i) => self.active_b = i == 1 && self.status.rx2.is_some(),
                Act::PlaceB(hz) => self.place_b(hz),
            }
        }
    }

    fn dropped(&mut self, ctx: &egui::Context) {
        let files: Vec<std::path::PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if !files.is_empty() {
            self.replay_files(ctx, &files);
        }
    }

    /// Send capture files to the server's decoders.
    fn replay_files(&mut self, ctx: &egui::Context, files: &[std::path::PathBuf]) {
        match crate::wav::replay_body(files) {
            Ok((body, hz)) => {
                let hz = hz.unwrap_or(self.inspect_hz());
                if let Some(l) = &self.link {
                    l.upload(&format!("/api/sdr/replay?frequency_hz={hz}"), body, Get::Replay);
                    self.recorder_busy = Some("replay".into());
                }
            }
            Err(e) => self.toast(ctx, Tone::Err, format!("Load I/Q: {e}")),
        }
    }
}

fn sep(ui: &mut Ui) {
    let (r, _) = ui.allocate_exact_size(vec2(1.0, 18.0), Sense::hover());
    ui.painter().rect_filled(r, CornerRadius::ZERO, theme::line_2());
}

fn annunciator(ui: &mut Ui, label: &str, value: &str, color: egui::Color32) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.label(egui::RichText::new(label).font(theme::mono(10.5)).color(theme::text_3()));
        ui.label(egui::RichText::new(value).font(theme::mono_bold(11.0)).color(color));
    });
}

pub fn icon_button(ui: &mut Ui, glyph: &str, tip: &str) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(vec2(26.0, 24.0), Sense::click());
    let h = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    ui.painter().rect_filled(r, CornerRadius::same(4), alpha(theme::surface_4(), h));
    ui.painter().text(r.center(), Align2::CENTER_CENTER, glyph, theme::sans(14.0), theme::lerp(theme::text_2(), theme::text(), h));
    resp.on_hover_text(tip)
}

pub fn mode_idle_text(m: Mode) -> &'static str {
    match m {
        Mode::Wfm => "broadcast FM · classifier off",
        Mode::Am => "envelope detection · no classifier",
        Mode::Usb | Mode::Lsb => "sideband voice",
        Mode::P25 | Mode::Dmr | Mode::Nxdn => "searching for sync…",
        Mode::Auto => "every decoder running on this channel",
        Mode::Flex => "FLEX search",
        Mode::Scan => "scanning for voice…",
        Mode::Pager => "paging sweep",
        Mode::Packet => "packet and paging decoders",
        Mode::Nfm => "listening · classifier on",
    }
}

pub fn utc_hms() -> String {
    let secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("{:02}:{:02}:{:02}", (secs / 3600) % 24, (secs / 60) % 60, secs % 60)
}

pub fn local_hms(t: SystemTime) -> String {
    // Wall-clock UTC; the decode log is compared against UTC radio logs.
    let secs = t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("{:02}:{:02}:{:02}", (secs / 3600) % 24, (secs / 60) % 60, secs % 60)
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.tick(ctx);
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }

    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        self.prefs.save();
    }

    fn on_exit(&mut self) {
        let _ = self.player.stop_recording();
        self.prefs.save();
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::ink().to_normalized_gamma_f32()
    }
}

impl App {
    /// Take what the network delivered, poll, and read the keyboard.
    pub fn tick(&mut self, ctx: &egui::Context) {
        // For the house widgets, which have no App to ask.
        crate::widgets::set_reduce_motion(ctx, self.reduce_motion());
        let events: Vec<NetEvent> = match &self.link {
            Some(l) => l.events.try_iter().take(512).collect(),
            None => Vec::new(),
        };
        for ev in events {
            self.handle(ctx, ev);
        }
        let now = ctx.input(|i| i.time);
        self.rates(now);
        if self.playing.is_some() && self.player.clip_progress().is_none() {
            self.playing = None;
        }
        if self.have_status && matches!(self.conn, Conn::Live) && now - self.calls_polled > 4.0 {
            self.calls_polled = now;
            if let Some(l) = &self.link {
                l.get("/api/sdr/calls", Get::Calls);
            }
        }
        self.shortcuts(ctx);
        self.dropped(ctx);
        while let Ok(p) = self.picked.1.try_recv() {
            match p {
                Picked::Replay(files) => self.replay_files(ctx, &files),
                Picked::RecordDir(dir) => {
                    self.prefs.record_dir = Some(dir);
                    self.prefs.save();
                }
            }
        }
    }

    /// Lay out the window.
    pub fn draw(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        self.prefs.ui_scale = ctx.zoom_factor();
        static QA_FPS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *QA_FPS.get_or_init(|| std::env::var_os("USDR_QA_FPS").is_some()) {
            use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
            static N: AtomicU64 = AtomicU64::new(0);
            static T: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
            let n = N.fetch_add(1, Relaxed) + 1;
            let mut t = T.lock().unwrap();
            let start = *t.get_or_insert_with(Instant::now);
            if start.elapsed() >= Duration::from_secs(2) {
                eprintln!("ui frames/s {:.1} · repaint causes {:?}", n as f64 / start.elapsed().as_secs_f64(), ctx.repaint_causes());
                N.store(0, Relaxed);
                *t = Some(Instant::now());
            }
        }
        let live = self.have_status && matches!(self.conn, Conn::Live | Conn::Connecting { .. }) && self.link.is_some();
        // The title says what the receiver is on, for the taskbar and alt-tab.
        let title = if live {
            let (hz, mode) = match (&self.status.rx2, self.active_b) {
                (Some(b), true) => (b.freq_hz, b.mode),
                _ => (self.inspect_hz(), self.status.mode),
            };
            format!("{} {} · uSDR", crate::tuning::fmt_mhz(hz, 3), mode.label())
        } else {
            "uSDR".to_string()
        };
        if title != self.title {
            self.title = title.clone();
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
        }
        if !live {
            crate::screens::connect_screen(self, ui);
            crate::screens::settings_window(self, &ctx);
            self.toasts_ui(&ctx);
            return;
        }

        egui::Panel::top("masthead")
            .exact_size(44.0)
            .frame(egui::Frame::new().fill(theme::surface_1()).inner_margin(egui::Margin::symmetric(14, 0)).stroke(Stroke::new(1.0, theme::line())))
            .show(ui, |ui| {
                theme::paint_weave(ui.painter(), ui.max_rect().expand2(vec2(14.0, 0.0)));
                self.masthead(ui)
            });
        egui::Panel::bottom("statusbar")
            .exact_size(26.0)
            .frame(egui::Frame::new().fill(theme::surface_1()).inner_margin(egui::Margin::symmetric(14, 0)).stroke(Stroke::new(1.0, theme::line())))
            .show(ui, |ui| self.status_bar(ui));
        // The receiver bar: the receiver being driven, across the window.
        egui::Panel::top("receiver")
            .frame(egui::Frame::new().fill(theme::surface_1()).inner_margin(egui::Margin::symmetric(14, 8)).stroke(Stroke::new(1.0, theme::line())))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                crate::bar::receiver_bar(self, ui);
                // The index tick: the bar belongs to the active receiver; it
                // takes B's violet as the wash does.
                let r = ui.max_rect().expand2(vec2(14.0, 8.0));
                let b = ui.ctx().animate_bool_with_time(egui::Id::new("rx-b-wash"), self.active_b && self.status.rx2.is_some(), 0.2);
                ui.painter().rect_filled(Rect::from_min_size(r.left_top(), vec2(16.0, 2.0)), CornerRadius::ZERO, theme::lerp(theme::EMBER, theme::VIOLET, b));
            });
        // The toolbar's own items take about 880 px; the memories about 140
        // for their label and STORE and a fixed width a slot, so where they
        // go depends on the window alone, never on how many are stored.
        // The toolbar runs the width of the display, beside the column.
        let toolbar_room = ui.available_width() - if self.prefs.show_side { self.prefs.side_width } else { 0.0 };
        let slots = self.memories().len().max(8);
        let mem_w = 140.0 + (crate::bar::SLOT_W + 4.0) * slots as f32;
        self.memories_on_toolbar = toolbar_room >= 880.0 + mem_w;
        if self.prefs.show_memories && !self.memories_on_toolbar {
            egui::Panel::top("memories")
                .exact_size(30.0)
                .frame(egui::Frame::new().fill(theme::surface_1()).inner_margin(egui::Margin::symmetric(14, 4)).stroke(Stroke::new(1.0, theme::line())))
                .show(ui, |ui| crate::bar::memory_row(self, ui));
        }
        // The display is the instrument: the column and the dock may not
        // take more than about a third of the width or two fifths of the
        // height left for it, so a small window still has a waterfall.
        let side_max = (ui.available_width() * 0.3).clamp(260.0, 440.0);
        let dock_max = (ui.available_height() * 0.4).clamp(120.0, 600.0);
        if self.prefs.show_side {
            let resp = egui::Panel::right("side")
                .resizable(true)
                .default_size(self.prefs.side_width.min(side_max))
                .size_range(260.0..=side_max)
                .frame(egui::Frame::new().fill(theme::surface_1()).inner_margin(egui::Margin::same(10)))
                .show(ui, |ui| {
                    theme::paint_weave(ui.painter(), ui.max_rect().expand(10.0));
                    crate::panels::side(self, ui);
                });
            self.prefs.side_width = resp.response.rect.width();
        }
        if self.prefs.show_dock {
            let resp = egui::Panel::bottom("dock")
                .resizable(true)
                .default_size(self.prefs.dock_height.min(dock_max))
                .size_range(120.0..=dock_max)
                .frame(egui::Frame::new().fill(theme::surface_1()).inner_margin(egui::Margin::symmetric(10, 8)).stroke(Stroke::new(1.0, theme::line())))
                .show(ui, |ui| crate::dock::dock(self, ui));
            self.prefs.dock_height = resp.response.rect.height();
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::ink()).inner_margin(egui::Margin::same(0)))
            .show(ui, |ui| {
                egui::Frame::new()
                    .fill(theme::surface_1())
                    .inner_margin(egui::Margin::symmetric(10, 2))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.set_height(28.0);
                        crate::toolbar::toolbar(self, ui);
                    });
                self.panafall(ui);
            });
        crate::screens::settings_window(self, &ctx);
        crate::screens::help_window(self, &ctx);
        crate::screens::diagnostics_window(self, &ctx);
        self.toasts_ui(&ctx);
        if !matches!(self.conn, Conn::Live) {
            crate::screens::reconnect_veil(self, &ctx);
        }
    }

}

/// The gist of a driver's error: what it quotes as the reason, or else what
/// follows its last colon.
fn error_gist(e: &str) -> &str {
    if let Some(a) = e.rfind("error '")
        && let Some(b) = e[a + 7..].find('\'')
    {
        return &e[a + 7..a + 7 + b];
    }
    e.rsplit(": ").next().unwrap_or(e).trim()
}

/// A status-bar error: an outlined chip with the subsystem and the gist,
/// the full message on hover; `retry` says a click tries again.
fn error_chip(ui: &mut Ui, what: &str, e: &str, retry: bool) -> egui::Response {
    let text = format!("▲ {what}: {}{}", crate::widgets::ellipsize(ui.painter(), error_gist(e), theme::mono(10.5), 280.0), if retry { " · RETRY" } else { "" });
    let g = ui.painter().layout_no_wrap(text, theme::mono(10.5), theme::ERR);
    let (rect, resp) = ui.allocate_exact_size(g.size() + vec2(14.0, 6.0), if retry { egui::Sense::click() } else { egui::Sense::hover() });
    let hover = ui.ctx().animate_bool_with_time(resp.id.with("hv"), retry && resp.hovered(), 0.12);
    ui.painter().rect(rect, CornerRadius::same(3), alpha(theme::ERR, 0.08 + 0.10 * hover), Stroke::new(1.0, alpha(theme::ERR, 0.55)), egui::StrokeKind::Inside);
    ui.painter().galley(rect.min + vec2(7.0, 3.0), g, theme::ERR);
    resp.on_hover_text(if retry { format!("{e}\n\nClick to open the audio output again") } else { e.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{Req, Wire};
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    struct T {
        app: App,
        wire: Wire,
        ready: bool,
    }

    fn rsp_status() -> Status {
        Status {
            serial: "210304DD98".into(),
            tuner: "RSP1A".into(),
            driver: "SDRplay".into(),
            host: Some("172.16.0.216".into()),
            gain_min_db: 0.0,
            gain_max_db: 96.0,
            gain_db: Some(22.0),
            rates_hz: vec![250e3, 500e3, 1e6, 2e6, 5e6],
            tune_min_hz: 1e3,
            tune_max_hz: 2.2e9,
            freq_hz: 146e6,
            rate_hz: 2e6,
            min_freq_hz: 145e6,
            max_freq_hz: 147e6,
            inspect_hz: 146.52e6,
            zoom: 1.0,
            bandwidth_hz: 12_500.0,
            mode: Mode::Nfm,
            bias_t: Some(true),
            rf_notch: Some(false),
            avg: Some("off".into()),
            ..Default::default()
        }
    }

    /// The whole window, connected to a server played by the test.
    fn harness() -> Harness<'static, T> {
        harness_sized(1600.0)
    }

    fn harness_sized(w: f32) -> Harness<'static, T> {
        let ctx = egui::Context::default();
        let prefs = Prefs { auto_connect: false, ..Default::default() };
        let mut app = App::build(&ctx, prefs, Player::silent(), Player::silent(), None);
        let (link, wire) = Link::wire();
        app.link = Some(link);
        let info = proto::ServerInfo { name: "usdr".into(), version: "test".into(), api: 2, features: vec!["filter".into(), "rx2".into()] };
        wire.events.send(NetEvent::Connected(Box::new(rsp_status()), Some(info))).unwrap();
        let mut h = Harness::builder().with_size(egui::vec2(w, 1000.0)).build_ui_state(
            |ui, t: &mut T| {
                let ctx = ui.ctx().clone();
                if !t.ready {
                    theme::install(&ctx, t.app.prefs.ground);
                    t.ready = true;
                    return;
                }
                t.app.tick(&ctx);
                t.app.draw(ui);
            },
            T { app, wire, ready: false },
        );
        h.run_steps(3);
        drain(&h);
        h
    }

    /// The posts the app made since the last call, as (path, body).
    fn drain(h: &Harness<'static, T>) -> Vec<(String, Value)> {
        h.state()
            .wire
            .reqs
            .try_iter()
            .filter_map(|r| match r {
                Req::Post { path, body, .. } | Req::PostGet { path, body, .. } => Some((path, body)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn mode_keys_post_the_mode() {
        let mut h = harness();
        h.get_by_label("WFM").click();
        h.run_steps(2);
        assert!(drain(&h).contains(&("/api/sdr/mode".into(), json!({ "mode": "wfm" }))));
    }

    #[test]
    fn agc_key_hands_gain_to_the_loop_and_back() {
        let mut h = harness();
        h.get_by_label("AGC").scroll_to_me();
        h.run_steps(3);
        h.get_by_label("AGC").click();
        h.run_steps(2);
        assert!(drain(&h).contains(&("/api/sdr/gain".into(), json!({ "gain_db": null }))));
    }

    #[test]
    fn front_end_switches_post_their_own_field() {
        let mut h = harness();
        h.get_by_label("BIAS-T").scroll_to_me();
        h.run_steps(3);
        h.get_by_label("BIAS-T").click();
        h.run_steps(2);
        assert!(drain(&h).contains(&("/api/sdr/frontend".into(), json!({ "bias_t": false }))));
    }

    #[test]
    fn averaging_posts_the_server_mode() {
        let mut h = harness();
        h.get_by_label("2 s").scroll_to_me();
        h.run_steps(3);
        h.get_by_label("2 s").click();
        h.run_steps(2);
        assert!(drain(&h).contains(&("/api/sdr/avg".into(), json!({ "mode": "deep" }))));
    }

    #[test]
    fn storing_a_memory_shares_the_bank() {
        let mut h = harness();
        h.get_by_label("STORE").click();
        h.run_steps(2);
        h.get_by_label("WX").click();
        h.run_steps(2);
        let posts = drain(&h);
        let (_, body) = posts.iter().find(|(p, _)| p == "/api/memories").expect("bank posted");
        let wx = &body["memories"][3];
        assert_eq!(wx["freq_hz"], json!(146_520_000.0));
        assert_eq!(wx["mode"], json!("nfm"));
        assert_eq!(body["memories"].as_array().unwrap().len(), 8);
    }

    #[test]
    fn recalling_a_memory_tunes_and_switches_mode() {
        let mut h = harness();
        h.get_by_label("FM BC").click();
        h.run_steps(2);
        let posts = drain(&h);
        assert!(posts.contains(&("/api/sdr/tune".into(), json!({ "freq_hz": 98_100_000.0 }))), "{posts:?}");
        assert!(posts.contains(&("/api/sdr/inspect".into(), json!({ "freq_hz": 98_100_000.0 }))));
        assert!(posts.contains(&("/api/sdr/mode".into(), json!({ "mode": "wfm" }))));
    }

    #[test]
    fn tuning_inside_the_span_leaves_the_center_alone() {
        let mut h = harness();
        h.state_mut().app.tune(146.3e6, false);
        let posts = drain(&h);
        assert_eq!(posts, vec![("/api/sdr/inspect".into(), json!({ "freq_hz": 146.3e6 }))]);
        assert_eq!(h.state().app.inspect_hz(), 146.3e6);
    }

    #[test]
    fn starting_a_scan_sends_the_range_then_the_mode() {
        let mut h = harness();
        h.state_mut().app.dock_tab = DockTab::Scan;
        h.run_steps(2);
        // The form scrolls in a short dock.
        h.get_by_label("Start scan").scroll_to_me();
        h.run_steps(3);
        h.get_by_label("Start scan").click();
        h.run_steps(2);
        let posts = drain(&h);
        let i = posts.iter().position(|(p, _)| p == "/api/sdr/scan/config").expect("config");
        let j = posts.iter().position(|(p, b)| p == "/api/sdr/mode" && b["mode"] == "scan").expect("mode");
        assert!(i < j);
        assert_eq!(posts[i].1["start_hz"], json!(450e6));
    }

    #[test]
    fn filter_presets_post_a_centered_width() {
        let mut h = harness();
        let range = proto::FilterRange { min_width_hz: 2_500.0, max_width_hz: 25_000.0, max_edge_hz: 12_500.0, symmetric: true };
        h.state_mut().app.modes = vec![ModeCaps { mode: Mode::Nfm, filter: Some(range), ..Default::default() }];
        h.run_steps(2);
        let role = egui::accesskit::Role::CheckBox;
        h.get_by_role_and_label(role, "12.5k").scroll_to_me();
        h.run_steps(3);
        h.get_by_role_and_label(role, "12.5k").click();
        h.run_steps(2);
        assert!(drain(&h).contains(&("/api/sdr/filter".into(), json!({ "low_hz": -6250.0, "high_hz": 6250.0 }))));
    }

    #[test]
    fn span_lock_keeps_the_tuner_still() {
        let mut h = harness();
        h.state_mut().app.prefs.span_lock = true;
        // A dial turn would normally recenter; locked it only moves the receiver.
        h.state_mut().app.tune(146.9e6, true);
        assert_eq!(drain(&h), vec![("/api/sdr/inspect".into(), json!({ "freq_hz": 146.9e6 }))]);
        // Out of the span, it still has to move.
        h.state_mut().app.tune(150e6, false);
        let posts = drain(&h);
        assert!(posts.contains(&("/api/sdr/tune".into(), json!({ "freq_hz": 150e6 }))), "{posts:?}");
    }

    #[test]
    fn receiver_b_starts_where_middle_clicked_and_takes_the_gestures_when_active() {
        let mut h = harness();
        let pwr = vec![-100.0f32; 1024];
        h.state().wire.events.send(NetEvent::Fft(Box::new(FftFrame { center_hz: 146e6, rate_hz: 2e6, inspect_hz: 146.52e6, pwr, ..Default::default() }))).unwrap();
        h.run_steps(3);
        let r = h.state().app.pan.regions.unwrap()[0];
        let x = r.left() + 0.25 * r.width(); // 145.5 MHz
        let p = egui::pos2(x, r.center().y);
        h.event(egui::Event::PointerMoved(p));
        h.event(egui::Event::PointerButton { pos: p, button: egui::PointerButton::Middle, pressed: true, modifiers: egui::Modifiers::NONE });
        h.event(egui::Event::PointerButton { pos: p, button: egui::PointerButton::Middle, pressed: false, modifiers: egui::Modifiers::NONE });
        h.run_steps(2);
        let posts = drain(&h);
        let (_, body) = posts.iter().find(|(p, _)| p == "/api/sdr/rx2").expect("rx2 posted");
        assert!((body["freq_hz"].as_f64().unwrap() - 145.5e6).abs() < 7_000.0);
        assert_eq!(body["mode"], json!("nfm"));
        // Made active, tuning goes to B and leaves A alone.
        h.state_mut().app.active_b = true;
        h.state_mut().app.tune(146.1e6, true);
        let posts = drain(&h);
        assert_eq!(posts, vec![("/api/sdr/rx2".into(), json!({ "freq_hz": 146.1e6, "mode": "nfm" }))]);
    }

    #[test]
    fn an_error_chip_says_the_gist() {
        assert_eq!(error_gist("Default Audio Device: ALSA function 'snd_pcm_open' failed with error 'Host is down (112)'"), "Host is down (112)");
        assert_eq!(error_gist("pipewire: no audio output device"), "no audio output device");
        assert_eq!(error_gist("no audio output device"), "no audio output device");
    }

    #[test]
    fn the_receiver_bar_follows_receiver_b() {
        let mut h = harness();
        let range = proto::FilterRange { min_width_hz: 2_000.0, max_width_hz: 20_000.0, max_edge_hz: 10_000.0, symmetric: true };
        h.state_mut().app.modes = vec![ModeCaps { mode: Mode::Am, filter: Some(range), bandwidth_hz: 10_000.0, ..Default::default() }];
        h.state_mut().app.status.rx2 = Some(proto::SubRx { freq_hz: 146.1e6, mode: Mode::Nfm, bandwidth_hz: 25_000.0, ..Default::default() });
        h.state_mut().app.active_b = true;
        h.run_steps(3);
        // B's modes are the listener's five; P25 is not among them.
        let role = egui::accesskit::Role::CheckBox;
        // (The one P25 key left is the decode log's filter.)
        assert_eq!(h.query_all_by_role_and_label(role, "P25").count(), 1);
        h.get_by_role_and_label(role, "AM").click();
        h.run_steps(2);
        assert!(drain(&h).contains(&("/api/sdr/rx2".into(), json!({ "freq_hz": 146.1e6, "mode": "am" }))));
        // Its filter is set for its own mode.
        h.state_mut().app.status.rx2.as_mut().unwrap().mode = Mode::Am;
        h.run_steps(2);
        // (A narrow bar keeps the presets beside B's 25 kHz: 12k, 16k.)
        h.get_by_role_and_label(role, "12k").click();
        h.run_steps(2);
        assert!(drain(&h).contains(&("/api/sdr/filter".into(), json!({ "low_hz": -6000.0, "high_hz": 6000.0, "mode": "am" }))));
        // AM's own 10k, a step narrower, is B's reset.
        h.state_mut().app.status.rx2.as_mut().unwrap().bandwidth_hz = 12_000.0;
        h.run_steps(2);
        h.get_by_label("narrower").click();
        h.run_steps(2);
        assert!(drain(&h).contains(&("/api/sdr/filter".into(), json!({ "reset": true, "mode": "am" }))));
        // And A comes back with a click.
        h.get_by_role_and_label(role, "A").click();
        h.run_steps(2);
        assert!(!h.state().app.active_b);
    }

    /// The top of the side column's card titled `title`, after a few frames.
    fn card_top(h: &mut Harness<'static, T>, title: &str) -> f32 {
        h.run_steps(4);
        h.get_by_label(title).rect().top()
    }

    #[test]
    fn the_side_column_holds_still_as_rds_arrives() {
        let mut h = harness();
        h.state_mut().app.status.mode = Mode::Wfm;
        let base = proto::Rds { pi: "81C7".into(), pi_call: Some("WRCH".into()), locked: true, pilot: true, bler: 0.02, ..Default::default() };
        let states = [
            None,
            Some(base.clone()),
            Some(proto::Rds { ps: Some("Breaking".into()), ps_dynamic: true, rt_partial: Some("Matthew Wil\u{2591}\u{2591}r - Break".into()), rt_progress: 0.4, ..base.clone() }),
            Some(proto::Rds {
                ps: Some("Lite 100".into()),
                ps_steady: true,
                pty: Some(5),
                pty_name: Some("Soft Adult Hits".into()),
                tp: true,
                rt: Some("Matthew Wilder - Break My Stride, and a long tail to wrap the line twice over".into()),
                title: Some("Break My Stride".into()),
                artist: Some("Matthew Wilder".into()),
                clock_utc: Some("2026-11-03T19:32Z".into()),
                clock_offset_min: Some(-300),
                ..base.clone()
            }),
            Some(proto::Rds { locked: false, ..base.clone() }),
        ];
        let mut tops = Vec::new();
        for rds in states {
            h.state_mut().app.status.rds = rds;
            tops.push(card_top(&mut h, "GAIN"));
        }
        assert!(tops.windows(2).all(|w| w[0] == w[1]), "the Gain card moved: {tops:?}");
    }

    #[test]
    fn the_recorder_card_keeps_its_height() {
        let mut h = harness();
        let mut tops = Vec::new();
        for (busy, note, err) in [(None, None, None), (Some("iq"), None, None), (None, Some("replayed 20 s through the decoders: 3 frames"), None), (None, None, Some("no IQ for 3s"))] {
            let app = &mut h.state_mut().app;
            app.recorder_busy = busy.map(String::from);
            app.replay_note = note.map(String::from);
            app.status.error = err.map(String::from);
            tops.push(card_top(&mut h, "RADIO"));
        }
        assert!(tops.windows(2).all(|w| w[0] == w[1]), "the Radio card moved: {tops:?}");
    }

    #[test]
    fn memory_slots_hold_their_places() {
        let mut h = harness();
        h.run_steps(3);
        // Slot 8 is "FM BC" in the default bank.
        let before = h.get_by_label("FM BC").rect();
        let list = vec![
            crate::prefs::Memory { name: "NOAA WX".into(), freq_hz: 162.475e6, mode: Mode::Nfm },
            crate::prefs::Memory { name: "A".into(), freq_hz: 1.0e6, mode: Mode::Am },
            crate::prefs::Memory { name: "A long name for a memory".into(), freq_hz: 146.52e6, mode: Mode::Nfm },
        ];
        h.state_mut().app.save_memories(list);
        h.run_steps(3);
        // Three slots of other lengths, the rest empty: slot 8 has not moved.
        assert_eq!(h.get_by_label("empty slot 8").rect(), before);
    }

    #[test]
    fn dock_tabs_hold_still_as_the_logs_grow() {
        let mut h = harness();
        h.run_steps(2);
        let before = h.get_by_label("Scan").rect();
        for n in 0..120 {
            let ev = proto::DecodeEvent { protocol: "DMR".into(), kind: "burst".into(), summary: format!("frame {n}"), valid: true, fields: Default::default() };
            h.state().wire.events.send(NetEvent::Decode { inspect_hz: 146.52e6, event: ev, at: SystemTime::now() }).unwrap();
            if n == 8 || n == 9 || n == 99 || n == 119 {
                h.run_steps(2);
                assert_eq!(h.get_by_label("Scan").rect(), before, "after {} decodes", n + 1);
            }
        }
    }

    #[test]
    fn decode_rows_stack_with_their_columns_aligned() {
        let mut h = harness();
        for (p, k, sum) in [("DMR", "burst", "first frame"), ("POCSAG", "alphanumeric", "second frame")] {
            let ev = proto::DecodeEvent { protocol: p.into(), kind: k.into(), summary: sum.into(), valid: true, fields: Default::default() };
            h.state().wire.events.send(NetEvent::Decode { inspect_hz: 146.52e6, event: ev, at: SystemTime::now() }).unwrap();
        }
        h.run_steps(3);
        let (a, b) = (h.get_by_label("second frame").rect(), h.get_by_label("first frame").rect());
        // Newest on top, one under the other, the summaries in one column.
        assert!(b.top() >= a.bottom() - 1.0, "rows overlap or sit side by side: {a:?} {b:?}");
        assert!((a.left() - b.left()).abs() < 1.0, "summaries zig-zag: {a:?} {b:?}");
    }

    #[test]
    fn m_mutes_the_receiver_being_driven() {
        let mut h = harness();
        h.state_mut().app.status.rx2 = Some(proto::SubRx { freq_hz: 146.1e6, mode: Mode::Nfm, bandwidth_hz: 12_500.0, ..Default::default() });
        h.state_mut().app.active_b = true;
        let (a, b) = (h.state().app.prefs.muted, h.state().app.prefs.muted_b);
        h.key_press(egui::Key::M);
        h.run_steps(2);
        assert_eq!(h.state().app.prefs.muted, a, "A left alone");
        assert_eq!(h.state().app.prefs.muted_b, !b, "B muted");
        h.state_mut().app.active_b = false;
        h.key_press(egui::Key::M);
        h.run_steps(2);
        assert_eq!(h.state().app.prefs.muted, !a, "then A");
    }

    #[test]
    fn the_right_column_shows_the_radio_and_the_recorder_together() {
        let mut h = harness();
        h.run_steps(2);
        assert!(h.query_by_label("BIAS-T").is_some());
        assert!(h.query_by_label("SAVE I/Q").is_some());
        // The P25 key card only in P25.
        assert!(h.query_by_label("SAVE KEY").is_none());
    }

    #[test]
    fn a_refused_change_says_who_has_the_receiver() {
        let mut h = harness();
        h.state().wire.events.send(NetEvent::Refused("uSDR desktop on bench has the receiver".into())).unwrap();
        h.run_steps(2);
        assert!(h.state().app.toasts.iter().any(|t| t.text.contains("bench")));
    }

    #[test]
    fn display_width_reaches_the_server_once_it_settles() {
        let mut h = harness();
        for _ in 0..40 {
            h.run_steps(1);
            std::thread::sleep(Duration::from_millis(15));
        }
        // Sent once it settled (a window steady from its first frame sends
        // it during the harness's own start), and not again while it stays.
        let posts = drain(&h);
        assert!(!posts.iter().any(|(p, _)| p == "/api/sdr/display"), "sent again: {posts:?}");
        assert!(h.state().app.display_bins_sent > 500);
    }

    /// A receiver in `mode` with the filter at `lo`..`hi`, which the server
    /// lets the operator set within `min`..`max`.
    fn filtered(h: &mut Harness<'static, T>, mode: Mode, home: f32, (min, max): (f32, f32), (lo, hi): (f64, f64), custom: bool) {
        let app = &mut h.state_mut().app;
        app.modes = vec![proto::ModeCaps {
            mode,
            bandwidth_hz: home,
            filter: Some(proto::FilterRange { min_width_hz: min, max_width_hz: max, max_edge_hz: if matches!(mode, Mode::Usb | Mode::Lsb) { max } else { max / 2.0 }, symmetric: !matches!(mode, Mode::Usb | Mode::Lsb) }),
            ..Default::default()
        }];
        app.status.mode = mode;
        app.status.filter_low_hz = lo;
        app.status.filter_high_hz = hi;
        app.status.bandwidth_hz = hi - lo;
        app.status.passband_offset_hz = (lo + hi) / 2.0;
        app.status.filter_custom = custom;
        h.run_steps(2);
        drain(h);
    }

    #[test]
    fn a_narrow_filter_bay_still_steps_wider() {
        // The 1500-1640 px band gives the filter its narrowest bay: once
        // narrowed to 150k the wider presets used to fall off the row.
        let mut h = harness_sized(1560.0);
        filtered(&mut h, Mode::Wfm, 200_000.0, (50_000.0, 230_000.0), (-75_000.0, 75_000.0), true);
        h.get_by_label("wider").click();
        h.run_steps(2);
        assert!(drain(&h).contains(&("/api/sdr/filter".into(), json!({ "low_hz": -90000.0, "high_hz": 90000.0 }))));
        h.get_by_label("narrower").click();
        h.run_steps(2);
        assert!(drain(&h).iter().any(|(p, b)| p == "/api/sdr/filter" && b["high_hz"] == json!(75000.0)));
    }

    #[test]
    fn stepping_onto_the_modes_own_width_resets_it() {
        let mut h = harness_sized(1560.0);
        filtered(&mut h, Mode::Wfm, 200_000.0, (50_000.0, 230_000.0), (-90_000.0, 90_000.0), true);
        h.get_by_label("wider").click();
        h.run_steps(2);
        assert!(drain(&h).contains(&("/api/sdr/filter".into(), json!({ "reset": true }))));
    }

    #[test]
    fn the_modes_own_width_is_a_preset_that_resets() {
        // Sideband's own 2.5k is not among the round presets; it joins them.
        let mut h = harness_sized(1900.0);
        filtered(&mut h, Mode::Usb, 2_500.0, (200.0, 4_000.0), (300.0, 2_400.0), true);
        h.get_by_label("2.5k").click();
        h.run_steps(2);
        assert!(drain(&h).contains(&("/api/sdr/filter".into(), json!({ "reset": true }))));
        h.get_by_label("2.7k").click();
        h.run_steps(2);
        assert!(drain(&h).contains(&("/api/sdr/filter".into(), json!({ "low_hz": 300.0, "high_hz": 3000.0 }))));
    }
}
