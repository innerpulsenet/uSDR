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
    /// Which of the log's families it belongs to, if any.
    pub family: Option<crate::decoders::Family>,
    /// Its height in the log when last laid out; 0 until it has been.
    pub height: f32,
    /// When the log first showed it, for its arrival flash.
    pub born: Option<f64>,
}

impl DecodeRow {
    pub fn new(at: SystemTime, freq_hz: f64, ev: proto::DecodeEvent) -> Self {
        let key = format!("{} {} {}", ev.protocol, ev.kind, ev.summary).to_lowercase();
        let family = crate::decoders::family_of(&ev.protocol);
        Self { at, freq_hz, ev, open: false, key, family, height: 0.0, born: None }
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
    /// Where the decodes come from.
    Map,
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
    /// The family the decode log shows (`None`: all of them), and the one
    /// decoder within it, by its chip.
    pub decode_family: Option<crate::decoders::Family>,
    pub decode_chip: Option<&'static str>,
    /// The tab row had no room for the log's keys; they ride the family row.
    pub decode_keys_below: bool,
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
    /// The radiofax viewer and the session's pictures.
    pub fax: crate::fax::Fax,
    pub sstv: crate::sstvview::Sstv,
    pub map: crate::map::MapView,
    /// Feld Hell's columns, the newest last, `HELL_KEEP` at most.
    pub hell: std::collections::VecDeque<u8>,
    pub settings_page: crate::settings::Page,
    /// The server's station and its spot reporter (servers with `station`).
    pub station: Option<(proto::Station, proto::ReporterStatus)>,
    pub station_form: crate::settings::StationForm,
    pub help_open: bool,
    pub connect_input: String,
    pub splash: brand::Splash,
    /// Optimistic receive frequency until the server confirms it.
    pub inspect_pending: Option<(f64, Instant)>,
    /// The mode the operator just chose, until the server reports it.
    pub mode_pending: Option<(Mode, Instant)>,
    pub gain_ui: f64,
    pub zoom_ui: f64,
    pub span_pending: Option<(f64, f64)>,
    pub clip_until: f64,
    pub spur: Option<SpurCheck>,
    display_bins_sent: usize,
    /// The span lock last pushed to the server as its zoom hold; `None`
    /// until it has been on this connection.
    hold_sent: Option<bool>,
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
    /// Rolled up to the shade strip: how to unroll.
    pub shade: Option<crate::shade::Restore>,
    /// The window's frame as it was opened.
    pub chrome: crate::chrome::Chrome,
    /// The window is wide enough for the memories to ride on the toolbar.
    /// The window title as last set.
    title: String,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, server: Option<String>) -> App {
        let prefs = Prefs::load();
        let player = Player::new(prefs.audio_device.as_deref());
        let player_b = Player::new_idle(prefs.audio_device.as_deref());
        let mut app = App::build(&cc.egui_ctx, prefs, player, player_b, server.clone());
        let qa = std::env::var_os("USDR_QA_SIZE").is_some();
        app.chrome = crate::chrome::Chrome {
            own_titlebar: app.prefs.own_titlebar,
            mica: !qa && app.prefs.mica && crate::chrome::apply_mica(cc),
        };
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
        // The second receiver's output is closed until there is one.
        player_b.set_idle(true);
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
            decode_family: None,
            decode_chip: None,
            decode_keys_below: false,
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
            fax: crate::fax::Fax::new(),
            sstv: crate::sstvview::Sstv::default(),
            map: crate::map::MapView::default(),
            hell: std::collections::VecDeque::new(),
            settings_page: Default::default(),
            station: None,
            station_form: Default::default(),
            help_open: false,
            connect_input,
            splash: brand::Splash::default(),
            inspect_pending: None,
            mode_pending: None,
            gain_ui: 0.0,
            zoom_ui: 1.0,
            span_pending: None,
            clip_until: 0.0,
            spur: None,
            display_bins_sent: 0,
            hold_sent: None,
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
            shade: None,
            chrome: Default::default(),
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
        self.hold_sent = None;
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
        let keep = self.status.rx2.as_ref().filter(|b| b.mode == mode).map(|b| (b.bandwidth_hz, b.passband_offset_hz, b.filter_shape));
        let (bw, off, filter_shape) = keep.unwrap_or((bhi - blo, (blo + bhi) / 2.0, proto::FilterShape::Normal));
        self.status.rx2 = Some(proto::SubRx { freq_hz: hz, mode, bandwidth_hz: bw, passband_offset_hz: off, filter_shape, note: None });
        self.player_b.set_idle(false);
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
        self.player_b.set_idle(true);
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
        // Zoomed in, the server places the window (on the receiver, or with
        // span lock held still until the receiver leaves it); the span
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

    /// Put `hz` in the middle of the display: zoomed in with the window held
    /// (span lock), the window moves there; otherwise the span does.
    pub fn center_view(&mut self, hz: f64) {
        if self.status.zoom > 1.01 && self.status.zoom_hold {
            self.pan_zoom(hz);
        } else {
            self.center_span(hz);
        }
    }

    /// The mode is `to` now, by the operator or by another client: a step
    /// picked by hand goes back to the mode's own when the two modes do not
    /// share a raster. A 1 kHz step chosen on 80 m, kept in broadcast FM,
    /// let a click land anywhere within a station instead of on it.
    fn mode_became(&mut self, from: Mode, to: Mode) {
        self.fax.mode_became(to);
        if from != to && to == Mode::Wefax {
            // The picture so far, for a viewer opened mid-fax.
            if let Some(l) = &self.link {
                l.fetch("/api/sdr/fax", Fetch::Fax);
            }
        }
        if from != to && self.prefs.step_hz.is_some() && crate::tuning::raster_changes(from, to, self.inspect_hz()) {
            self.prefs.step_hz = None;
        }
    }

    pub fn set_mode(&mut self, m: Mode) {
        if m != Mode::Scan {
            self.prefs.last_voice_mode = m;
        }
        self.mode_became(self.status.mode, m);
        // Held against the statuses already on their way with the old mode
        // (a memory's tune answers before its mode change does), which put
        // the old mode's raster and keys back for a moment.
        self.mode_pending = Some((m, Instant::now()));
        self.status.mode = m;
        if let Some(f) = crate::decoders::Family::for_mode(m) {
            self.decode_family = Some(f);
            self.decode_chip = None;
        }
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

    /// The sideband filter's skirt, for the receiver being driven.
    pub fn set_filter_shape(&mut self, shape: proto::FilterShape) {
        match self.status.rx2.as_mut().filter(|_| self.active_b) {
            Some(b) => {
                b.filter_shape = shape;
                let mode = b.mode;
                self.post("/api/sdr/filter", json!({ "shape": shape.wire(), "mode": mode.wire() }));
            }
            None => {
                self.status.filter_shape = shape;
                self.post("/api/sdr/filter", json!({ "shape": shape.wire() }));
            }
        }
    }

    pub fn set_zoom(&mut self, z: f64) {
        let z = if z < 1.04 { 1.0 } else { z.min(32.0) };
        self.zoom_ui = z;
        self.post("/api/sdr/zoom", json!({ "zoom": z }));
    }

    /// Zoomed in, the window is the server's to place: it follows the
    /// receiver, or with span lock holds still while the receiver moves
    /// across it. The lock is this app's, so it is pushed when it changes
    /// and on each connection (a restarted server comes back following);
    /// another client's choice in between is left alone.
    fn sync_zoom_hold(&mut self) {
        let lock = self.prefs.span_lock;
        if !self.have_status || !matches!(self.conn, Conn::Live) || self.hold_sent == Some(lock) {
            return;
        }
        if self.status.zoom_hold != lock {
            self.post("/api/sdr/zoom", json!({ "hold": lock }));
        }
        self.hold_sent = Some(lock);
    }

    /// Zoomed in with span lock, move the held window to `hz`; the receiver
    /// stays where it is.
    pub fn pan_zoom(&mut self, hz: f64) {
        self.post("/api/sdr/zoom", json!({ "hold": true, "center_hz": hz }));
    }

    pub fn memories(&self) -> Vec<Memory> {
        self.memories.clone().unwrap_or_else(crate::prefs::default_memories)
    }

    pub fn save_memories(&mut self, list: Vec<Memory>) {
        self.memories = Some(list.clone());
        self.post("/api/memories", json!({ "memories": list }));
    }

    fn push_decode(&mut self, row: DecodeRow) {
        self.pan.ping(row.freq_hz, crate::widgets::protocol_color(&row.ev.protocol));
        self.decodes.push_front(row);
        self.decodes_added += 1;
        if self.decode_scroll > 1.0 {
            self.decodes_unseen += 1;
        }
        // FT8 on a busy band is forty decodes a slot; the log keeps a
        // quarter hour of that.
        self.decodes.truncate(1500);
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
                let fax_mode = s.mode == Mode::Wefax;
                self.apply_status(*s);
                if fax_mode {
                    self.fax.mode_became(Mode::Wefax);
                    if let Some(l) = &self.link {
                        l.fetch("/api/sdr/fax", Fetch::Fax);
                    }
                }
                self.display_bins_sent = 0;
                self.hold_sent = None;
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
                if let Some(l) = &self.link {
                    l.stats.took_fft();
                }
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
                if event.protocol == "WEFAX"
                    && let Some(i) = self.fax.event(&event)
                    && self.prefs.fax_autosave
                {
                    crate::fax::save(self, ctx, i);
                }
                // An SSTV picture's start names its mode for the viewer.
                if event.protocol == "SSTV"
                    && event.kind == "start"
                    && let (Some(id), Some(m)) = (event.fields.get("image").and_then(|v| v.parse().ok()), event.fields.get("mode"))
                {
                    self.sstv.named(id, m);
                }
                self.push_decode(DecodeRow::new(at, inspect_hz, event));
                if call && let Some(l) = &self.link {
                    l.get("/api/sdr/calls", Get::Calls);
                }
            }
            NetEvent::Station(st, rep) => self.station = Some((st, rep)),
            NetEvent::FaxLine(l) => {
                // The receiver's dial, not the span's center.
                let hz = self.status.inspect_hz;
                self.fax.line(l, hz);
            }
            NetEvent::HellColumns(h) => {
                self.hell.extend(h.pixels.iter().copied());
                let keep = crate::utilviews::HELL_KEEP * h.rows.max(1) as usize;
                while self.hell.len() > keep {
                    self.hell.pop_front();
                }
            }
            NetEvent::ImageLine(l) => {
                let hz = self.status.inspect_hz;
                self.sstv.line(l, hz);
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

    fn apply_status(&mut self, mut s: Status) {
        match self.mode_pending {
            Some((m, _)) if m == s.mode => self.mode_pending = None,
            Some((m, at)) if at.elapsed() < Duration::from_millis(1200) => s.mode = m,
            Some(_) => self.mode_pending = None,
            None => {}
        }
        if self.have_status {
            self.mode_became(self.status.mode, s.mode);
        }
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
        // Another client may have opened or closed the second receiver.
        self.player_b.set_idle(s.rx2.is_none());
        // A new mode opens the log on its own decodes, when it has a family.
        if (!self.have_status || s.mode != self.status.mode)
            && let Some(f) = crate::decoders::Family::for_mode(s.mode)
        {
            self.decode_family = Some(f);
            self.decode_chip = None;
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
            Fetch::Fax => {
                let id = header("x-usdr-fax-image").and_then(|v| v.parse().ok());
                let width = header("x-usdr-fax-width").and_then(|v| v.parse().ok()).unwrap_or(0);
                if let Some(id) = id {
                    let hz = self.status.inspect_hz;
                    self.fax.backfill(id, width, hz, &bytes);
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
                Key::F11 if self.shade.is_none() => {
                    self.fullscreen = !self.fullscreen;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
                }
                Key::F9 => self.toggle_shade(ctx),
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
            crate::anim::after(ctx, Duration::from_millis(450));
        }
    }

    // ---------- layout ----------

    fn masthead(&mut self, ui: &mut Ui) {
        let now = ui.input(|i| i.time);
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            let (r, mark) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::click());
            brand::mark_static(ui.painter(), r);
            let word = theme::steel_label(ui, "uSDR", theme::wordmark(20.0)).interact(Sense::click());
            if mark.double_clicked() || word.double_clicked() {
                self.toggle_shade(ui.ctx());
            }
            word.on_hover_text("Double-click to roll the window up to a strip (F9)");
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
                if self.chrome.own_titlebar {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    crate::chrome::window_keys(ui);
                    ui.spacing_mut().item_spacing.x = 10.0;
                    sep(ui);
                }
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
                // The dial scale takes the middle the two ends leave.
                let room = ui.available_width() - 28.0;
                if self.have_status && room >= crate::dialscale::MIN_W {
                    ui.add_space(14.0);
                    crate::dialscale::show(self, ui, room);
                }
            });
        });
        // The clock: a frame every half second whatever else is quiet.
        crate::anim::after(ui.ctx(), Duration::from_millis(500));
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
            if ui.button("Roll up to a strip   F9").clicked() {
                self.toggle_shade(ui.ctx());
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
                self.reporter_readout(ui);
                if let Some((path, secs)) = &a.recording {
                    small(ui, format!("● recording {secs:.1} s → {}", path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default()), theme::EMBER);
                }
            });
        });
    }

    /// The spot reporter on the status bar, while it is on: a lamp, the
    /// spots sent, the stations this hour and when the next upload goes.
    /// The count rolls as it climbs and the lamp flares at each upload.
    /// Hover for the whole of it; click for its settings.
    fn reporter_readout(&mut self, ui: &mut Ui) {
        let Some((st, rep)) = &self.station else { return };
        if rep.state == proto::ReporterState::Off {
            return;
        }
        let (lamp, word) = match rep.state {
            proto::ReporterState::Waiting => (theme::OK, "PSKREPORTER"),
            proto::ReporterState::Incomplete => (theme::WARN, "PSKREPORTER · NEEDS CALLSIGN"),
            _ => (theme::ERR, "PSKREPORTER · NOT SENDING"),
        };
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let next = match rep.next_send {
            Some(t) if rep.queued > 0 => {
                let s = t.saturating_sub(now);
                format!("next {}:{:02}", s / 60, s % 60)
            }
            _ => "next when heard".into(),
        };
        let count = format!("{}", rep.spots_sent);
        let tail = format!(" spots · {}/h · {next}", rep.heard_hour);
        let font = theme::mono(10.5);
        let p = ui.painter().clone();
        let w_word = p.layout_no_wrap(word.into(), font.clone(), theme::text_2()).size().x;
        // The count's width is held to its digits plus one, so a new digit
        // does not shove the readout along.
        let w_count = p.layout_no_wrap("0".repeat(count.len() + 1), font.clone(), theme::text()).size().x;
        let w_tail = p.layout_no_wrap(tail.clone(), font.clone(), theme::text_3()).size().x;
        let lamp_w = 12.0;
        let (rect, resp) = ui.allocate_exact_size(vec2(lamp_w + w_word + 8.0 + w_count + w_tail + 8.0, 18.0), Sense::click());
        if resp.hovered() {
            p.rect_filled(rect, CornerRadius::same(3), alpha(theme::surface_4(), 0.6));
        }
        // A flare when the count goes up: an upload just left.
        let flare_id = egui::Id::new("pskr-flare");
        let (seen, since): (u64, f64) = ui.data(|d| d.get_temp(flare_id)).unwrap_or((rep.spots_sent, f64::NEG_INFINITY));
        let t = ui.input(|i| i.time);
        let since = if rep.spots_sent > seen { t } else { since };
        ui.data_mut(|d| d.insert_temp(flare_id, (rep.spots_sent, since)));
        let flare = if self.reduce_motion() { 0.0 } else { (1.0 - ((t - since) / 1.6) as f32).clamp(0.0, 1.0) };
        if flare > 0.0 {
            crate::anim::more(ui.ctx());
        }
        let c = rect.left_center() + vec2(6.0, 0.0);
        if flare > 0.0 {
            p.circle_filled(c, 4.0 + 5.0 * flare, alpha(lamp, 0.25 * flare));
        }
        theme::lamp(&p, c, 3.2, lamp, 0.75 + 0.25 * flare, theme::LampShape::Round);
        let mut x = rect.left() + lamp_w;
        p.text(egui::pos2(x, rect.center().y), Align2::LEFT_CENTER, word, font.clone(), theme::text_2());
        x += w_word + 8.0;
        crate::widgets::rolling_text(&p, egui::Id::new("pskr-count"), egui::pos2(x + w_count, rect.center().y), Align2::RIGHT_CENTER, &count, theme::mono_bold(10.5), theme::text());
        x += w_count;
        p.text(egui::pos2(x, rect.center().y), Align2::LEFT_CENTER, &tail, font, theme::text_3());
        let utc = |t: u64| format!("{:02}:{:02} UTC", (t / 3600) % 24, (t / 60) % 60);
        let last = rep.last_sent.map_or("none yet".into(), utc);
        let err = rep.last_error.clone().map(|e| format!("\n{e}")).unwrap_or_default();
        let tip = format!(
            "Spots reported to PSKReporter as {} in {}.\n{} spots in {} uploads since the server started · {} stations this hour · {} waiting · last upload {last}{err}\n{} WSPR spots to WSPRnet{}\nClick for the station settings.",
            if st.callsign.is_empty() { "—" } else { &st.callsign },
            if st.grid.is_empty() { "—" } else { &st.grid },
            rep.spots_sent,
            rep.datagrams,
            rep.heard_hour,
            rep.queued,
            rep.wsprnet_sent,
            rep.wsprnet_error.as_ref().map(|e| format!(" · {e}")).unwrap_or_default()
        );
        let resp = resp.on_hover_text(tip);
        if resp.clicked() {
            self.settings_page = crate::settings::Page::Station;
            self.settings_open = true;
        }
        // The countdown moves once a second.
        crate::anim::after(ui.ctx(), std::time::Duration::from_secs(1));
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
                        .shadow({
                            let s = theme::shadow(theme::Depth::Floating);
                            egui::Shadow { color: s.color.gamma_multiply(fade), ..s }
                        })
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

    /// The receiver flag's line: what the HF data decoder hears; else what
    /// the classifier has named, live or just held; else what the frequency
    /// is allocated to, and whether a signal stands clear of the noise
    /// there. Never the classifier's "Idle / Noise", which it also says in
    /// every mode it does not run in.
    pub fn flag_line(&self) -> String {
        if let Some(t) = self.hf_flag_text() {
            return t;
        }
        let (c, _) = self.shown_class();
        if let Some(c) = c.filter(|c| c.active && !matches!(c.protocol.as_str(), "Idle / Noise" | "Noise" | "FLEX Search" | "") && !c.protocol.starts_with("Unclassified")) {
            return c.protocol;
        }
        let present = self.meter.snr.is_some_and(|s| s >= 10.0);
        crate::bandplan::describe(self.inspect_hz(), self.status.mode, present)
    }

    /// What the HF data decoder hears, for the receiver's flag: one line in
    /// place of the classifier's, which has nothing to say about sideband.
    /// FT8 and FT4 count the stations in the newest slot, PSK31 the signals
    /// it is copying. Short, so the SNR after it stays on the card.
    fn hf_flag_text(&self) -> Option<String> {
        let mode = self.status.mode;
        if !mode.is_hf_data() {
            return None;
        }
        let r = self.status.digital.clone().unwrap_or_default();
        let plural = |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        Some(match mode {
            Mode::Ft8 | Mode::Ft4 => {
                // The decodes arrive newest first and a slot's together, so
                // the newest slot is the run at the front of the log.
                let slot_of = |d: &DecodeRow| d.ev.fields.get("slotMs").and_then(|v| v.parse::<u64>().ok());
                let mut rows = self.decodes.iter().filter(|d| d.ev.protocol == mode.label());
                let Some(newest) = rows.next() else { return Some(format!("{} · listening", mode.label())) };
                let slot = slot_of(newest);
                let now_ms = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
                let period_ms = if mode == Mode::Ft8 { 15_000 } else { 7_500 };
                // Two slots without a decode, and the band has gone quiet.
                if slot.is_none_or(|s| now_ms.saturating_sub(s) > 3 * period_ms) {
                    return Some(format!("{} · quiet", mode.label()));
                }
                let mut calls = std::collections::HashSet::new();
                for d in std::iter::once(newest).chain(rows).take_while(|d| slot_of(d) == slot) {
                    calls.insert(d.ev.fields.get("de").cloned().unwrap_or_else(|| d.ev.summary.clone()));
                }
                format!("{} · {}", mode.label(), plural(calls.len(), "station", "stations"))
            }
            Mode::Psk31 => {
                let n = r.channels.iter().filter(|c| c.active).count();
                if n == 0 { "PSK31 · listening".into() } else { format!("PSK31 · copying {}", plural(n, "signal", "signals")) }
            }
            Mode::Rtty | Mode::Navtex => match (r.locked, r.center_hz) {
                (true, Some(hz)) if r.in_message => format!("{} · message at {hz:.0} Hz", mode.label()),
                (true, Some(hz)) => format!("{} · copying at {hz:.0} Hz", mode.label()),
                _ => format!("{} · listening", mode.label()),
            },
            Mode::Wefax => match r.fax_state.as_deref() {
                Some("receiving") => format!("WEFAX · row {}", r.fax_lines),
                Some("phasing") => "WEFAX · phasing".into(),
                Some("starting") => "WEFAX · start tone".into(),
                _ => "WEFAX · waiting".into(),
            },
            // The rest say what the decoder found the signal to be.
            m => match (r.locked, r.variant.as_deref()) {
                (true, Some(v)) => format!("{} · {v}", m.label()),
                (true, None) => format!("{} · copying", m.label()),
                (false, _) => format!("{} · listening", m.label()),
            },
        })
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
            protocol: Some(self.flag_line()),
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
                Act::Center(hz) => self.center_view(hz),
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

/// The faceplate's margin around the display, points.
const BEZEL: f32 = 6.0;
/// Every row's distance from the window's edge, so their ends line up.
const EDGE: i8 = 12;

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
    let (press, c) = widgets::paint_key(ui, &resp, r, CornerRadius::same(4), false, theme::AMBER);
    ui.painter().text(r.center() + vec2(0.0, press), Align2::CENTER_CENTER, glyph, theme::sans(14.0), c);
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
        Mode::Ft8 => "FT8 · decoding each 15 s slot",
        Mode::Ft4 => "FT4 · decoding each 7.5 s slot",
        Mode::Wefax => "radiofax · waiting for a start tone",
        Mode::Rtty => "RTTY · finding the tone pair",
        Mode::Navtex => "NAVTEX · SITOR-B, 100 baud",
        Mode::Psk31 => "PSK31 · every signal in the passband",
        Mode::Sitor => "SITOR · ARQ or FEC, 100 baud",
        Mode::Dsc => "DSC · listening for calls",
        Mode::Hfdl => "HFDL · listening for bursts",
        Mode::Pactor => "PACTOR-I · listening",
        Mode::Stanag => "STANAG · looking for a serial tone",
        Mode::Ale => "ALE · listening for soundings and calls",
        Mode::Cw => "CW · every signal in the passband",
        Mode::Wspr => "WSPR · decoding each 2-minute slot",
        Mode::Fst4w => "FST4W · decoding every period",
        Mode::Js8 => "JS8 · decoding each slot, all four speeds",
        Mode::Sstv => "SSTV · waiting for a VIS code",
        Mode::Olivia => "Olivia · trying its common settings",
        Mode::Contestia => "Contestia · trying its common settings",
        Mode::Dominoex => "DominoEX · searching every speed",
        Mode::Thor => "Thor · searching every speed",
        Mode::Mt63 => "MT63 · trying its common settings",
        Mode::Selcal => "SELCAL · listening for tone pairs",
        Mode::Mfsk => "MFSK · listening",
        Mode::Hell => "Feld Hell · painting",
        Mode::Wwv => "WWV · reading the time code",
        Mode::Drm => "DRM · finding the multiplex",
        Mode::Nfm => "listening · classifier on",
        Mode::Ais => "AIS · both channels, 161.975 and 162.025",
    }
}

/// A Unix time as UTC `YYYYMMDD` and `HHMMSS`, for file names.
pub fn utc_date_time(secs: u64) -> (String, String) {
    // Civil from days (Howard Hinnant's algorithm), so no time crate is needed.
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let s = secs % 86_400;
    (format!("{y:04}{m:02}{d:02}"), format!("{:02}{:02}{:02}", s / 3600, (s / 60) % 60, s % 60))
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
        if self.chrome.mica {
            return [0.0; 4];
        }
        let c = theme::ink();
        if theme::night() { theme::night_color(c) } else { c }.to_normalized_gamma_f32()
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
        self.sync_zoom_hold();
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
        crate::anim::frame_begin();
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
                let a = self.player.reading();
                eprintln!(
                    "ui frames/s {:.1} · audio queue {:.0} ms, {} underruns · loop released {} times, {} spectrum frames let go · motion asked by {:?} · repaint causes {:?}",
                    n as f64 / start.elapsed().as_secs_f64(),
                    a.queue_ms,
                    a.underruns,
                    crate::anim::releases(),
                    self.link.as_ref().map_or(0, |l| l.stats.fft_dropped.load(std::sync::atomic::Ordering::Relaxed)),
                    crate::anim::asked(),
                    ctx.repaint_causes()
                );
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
        if self.chrome.own_titlebar {
            crate::chrome::resize_edges(&ctx);
            if !ctx.input(|i| i.viewport().maximized.unwrap_or(false)) {
                crate::chrome::outline(&ctx);
            }
        }
        if self.shade.is_some() {
            crate::shade::strip(self, ui);
            return;
        }
        if !live && self.chrome.own_titlebar {
            // No masthead before a connection: a bare title bar, so the
            // window can still be moved and closed.
            egui::Panel::top("titlebar")
                .exact_size(36.0)
                .frame(egui::Frame::new().fill(self.chrome.panel_fill()).inner_margin(egui::Margin::symmetric(14, 0)))
                .show(ui, |ui| {
                    crate::chrome::title_drag(ui, ui.max_rect().expand2(vec2(14.0, 0.0)));
                    ui.horizontal_centered(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                        brand::mark_static(ui.painter(), r);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), crate::chrome::window_keys);
                    });
                });
        }
        if !live {
            crate::screens::connect_screen(self, ui);
            crate::screens::settings_window(self, &ctx);
            self.toasts_ui(&ctx);
            return;
        }

        // The whole window is one plate: the panels below are clear over it,
        // grooves between them where the plate is cut, the displays windows
        // let into it.
        theme::paint_plate(ui.painter(), ctx.viewport_rect(), self.chrome.plate_fill());
        let r = egui::Panel::top("masthead")
            .exact_size(44.0)
            .show_separator_line(false)
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(EDGE, 0)))
            .show(ui, |ui| {
                if self.chrome.own_titlebar {
                    crate::chrome::title_drag(ui, ui.max_rect().expand2(vec2(EDGE as f32, 0.0)));
                }
                self.masthead(ui)
            })
            .response
            .rect;
        theme::groove_h(ui.painter(), r.left(), r.right(), r.bottom() - 2.0);
        let r = egui::Panel::bottom("statusbar")
            .exact_size(26.0)
            .show_separator_line(false)
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(EDGE, 0)))
            .show(ui, |ui| self.status_bar(ui))
            .response
            .rect;
        theme::groove_h(ui.painter(), r.left(), r.right(), r.top());
        // The receiver bar: the receiver being driven, across the window.
        let r = egui::Panel::top("receiver")
            .show_separator_line(false)
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(EDGE, 8)))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                crate::bar::receiver_bar(self, ui);
                // The index tick: the bar belongs to the active receiver; it
                // takes B's violet as the wash does.
                let r = ui.max_rect().expand2(vec2(EDGE as f32, 8.0));
                let on_b = self.active_b && self.status.rx2.is_some();
                let b = ui.ctx().animate_bool_with_time(egui::Id::new("rx-b-wash"), on_b, 0.2);
                ui.painter().rect_filled(Rect::from_min_size(r.left_top(), vec2(16.0, 2.0)), CornerRadius::ZERO, theme::lerp(theme::EMBER, theme::VIOLET, b));
                // The edge light: amber for A, violet for B. On a switch it
                // runs out across the bar and back as its color turns.
                let rm = self.reduce_motion();
                let sweep = if rm { if on_b { 1.0 } else { 0.0 } } else { ui.ctx().animate_bool_with_time(egui::Id::new("rx-edge-sweep"), on_b, 0.45) };
                let travel = (sweep * std::f32::consts::PI).sin() * 0.6;
                theme::edge_light(ui.painter(), r.shrink2(vec2(0.0, 0.0)), theme::lerp(theme::AMBER, theme::VIOLET, b), travel);
            })
            .response
            .rect;
        theme::groove_h(ui.painter(), r.left(), r.right(), r.bottom() - 2.0);
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
                .show_separator_line(false)
                .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(EDGE, 8)))
                .show(ui, |ui| crate::panels::side(self, ui));
            let r = resp.response.rect;
            theme::groove_v(ui.painter(), r.left(), r.top() + 6.0, r.bottom() - 6.0);
            self.prefs.side_width = r.width();
        }
        if self.prefs.show_dock {
            let resp = egui::Panel::bottom("dock")
                .resizable(true)
                .default_size(self.prefs.dock_height.min(dock_max))
                .size_range(120.0..=dock_max)
                .show_separator_line(false)
                .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(EDGE, 8)))
                .show(ui, |ui| crate::dock::dock(self, ui));
            self.prefs.dock_height = resp.response.rect.height();
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().inner_margin(egui::Margin::same(0)))
            .show(ui, |ui| {
                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(EDGE, 2))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.set_height(28.0);
                        crate::toolbar::toolbar(self, ui);
                    });
                // The display let into the plate behind a bezel, its edges
                // on the same line as the keys above and the windows below.
                let plate = ui.available_rect_before_wrap();
                let screen = plate.shrink2(vec2(EDGE as f32, BEZEL));
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(screen));
                self.panafall(&mut child);
                theme::bezel(ui.painter(), screen, BEZEL);
                ui.advance_cursor_after_rect(plate);
            });
        crate::screens::settings_window(self, &ctx);
        crate::screens::help_window(self, &ctx);
        crate::fax::window(self, &ctx);
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

    #[test]
    fn the_dock_fits_its_panel_in_the_smallest_window() {
        // A family with its own decoders, a filter and the scope, all in a
        // 1100 x 700 window: content taller than the dock slid it down
        // under the display, which hid its tabs.
        // The scope as wide as an operator leaves it, so the log is narrow.
        let mut h = harness_with(Prefs { auto_connect: false, scope_width: 420.0, ..Default::default() }, 1100.0, 700.0);
        h.state_mut().app.status.mode = Mode::Wfm;
        h.state_mut().app.decode_family = Some(crate::decoders::Family::Broadcast);
        h.run_steps(4);
        let dock_top = 700.0 - 26.0 - h.state().app.prefs.dock_height;
        let tab = h.get_by_label("Decodes").rect();
        assert!(tab.top() >= dock_top && tab.top() <= dock_top + 12.0, "tab at {tab:?}, dock from {dock_top}");
        // The display ends where the dock begins.
        assert!(h.get_by_label("CENTER").rect().bottom() < dock_top);
    }

    #[test]
    fn the_receiver_bar_never_cuts_off_its_signal_zone() {
        // CAL is the last thing on the signal zone's top line: it stays
        // clear of the reading on its left and the audio zone on its right.
        for w in [1100.0, 1280.0, 1400.0, 1500.0, 1600.0, 1760.0, 1900.0, 2560.0] {
            for with_b in [false, true] {
                let mut h = harness_sized(w);
                h.run_steps(2);
                h.state_mut().app.status.freq_error_hz = Some(1_900.0);
                if with_b {
                    h.state_mut().app.status.rx2 = Some(proto::SubRx { freq_hz: 146.1e6, mode: Mode::Nfm, bandwidth_hz: 12_500.0, ..Default::default() });
                }
                h.run_steps(4);
                let cal = h.get_by_label("CAL +1.9k").rect();
                let signal = h.get_by_label("SIGNAL").rect();
                let audio = h.get_by_label("AUDIO A").rect();
                assert!(cal.right() < audio.left(), "{w} px{}: the CAL key ends at {}, audio starts at {}", if with_b { " with B" } else { "" }, cal.right(), audio.left());
                // Room between the label and the key for S9+60 and -100.0 dBFS.
                assert!(cal.left() - signal.right() >= 120.0, "{w} px: the CAL key at {} crowds the reading after {}", cal.left(), signal.right());
            }
        }
    }

    #[test]
    fn a_view_that_hides_every_frame_says_so_and_shows_them_back() {
        let mut h = harness();
        h.run_steps(2);
        let event = proto::DecodeEvent { protocol: "P25".into(), kind: "voice header".into(), summary: "group call".into(), valid: true, fields: Default::default() };
        h.state().wire.events.send(NetEvent::Decode { inspect_hz: 146.04e6, event, at: std::time::SystemTime::now() }).unwrap();
        h.state_mut().app.decode_family = Some(crate::decoders::Family::Broadcast);
        h.run_steps(3);
        h.get_by_label("1 frame in the log is outside this view.");
        h.get_by_label("SHOW ALL").click();
        h.run_steps(2);
        assert_eq!(h.state().app.decode_family, None);
        assert_eq!(h.query_all_by_label("SHOW ALL").count(), 0);
    }

    fn harness_sized(w: f32) -> Harness<'static, T> {
        harness_wh(w, 1000.0)
    }

    fn harness_wh(w: f32, ht: f32) -> Harness<'static, T> {
        harness_with(Prefs { auto_connect: false, ..Default::default() }, w, ht)
    }

    #[test]
    fn rds_text_coming_and_going_never_moves_the_receiver_bar() {
        // Broadcast FM with RDS whose RadioText arrives, grows and changes:
        // the receiver readout and the bar's height hold still throughout.
        for w in [1400.0, 1600.0, 1760.0, 1900.0] {
            let mut h = harness_wh(w, 1000.0);
            {
                let app = &mut h.state_mut().app;
                app.status.mode = Mode::Wfm;
                app.status.inspect_hz = 100.5e6;
                app.status.freq_hz = 100.5e6;
                app.status.bandwidth_hz = 200e3;
            }
            h.run_steps(4);
            let lcd = h.get_by_label("RECEIVER A").rect();
            let texts = ["", "WIYY", "98 Rock · Now playing", "98 Rock · Now playing METALLICA - ENTER SANDMAN on the station that rocks Baltimore all day long", "short"];
            for (k, t) in texts.iter().cycle().take(15).enumerate() {
                {
                    let app = &mut h.state_mut().app;
                    app.status.rds = Some(proto::Rds {
                        pi: "1234".into(),
                        ps: Some("WIYY".into()),
                        ps_steady: true,
                        rt: (!t.is_empty()).then(|| t.to_string()),
                        rt_partial: (k % 2 == 1).then(|| format!("{t}░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░")),
                        rt_progress: 0.5,
                        ..Default::default()
                    });
                }
                h.run_steps(2);
                let now = h.get_by_label("RECEIVER A").rect();
                assert_eq!(now, lcd, "{w}: the readout moved with RDS text {t:?} (step {k})");
            }
        }
    }

    #[test]
    fn the_dial_scale_tunes_where_it_is_clicked_and_dragged() {
        let mut h = harness_wh(1900.0, 1000.0);
        {
            let app = &mut h.state_mut().app;
            (app.status.tune_min_hz, app.status.tune_max_hz) = (1e3, 2e9);
        }
        h.run_steps(3);
        drain(&h);
        let r = h.get_by_label("dial scale").rect();
        // Log scale, 1 kHz to 2 GHz: 10 MHz sits at this fraction.
        let frac = |hz: f64| ((hz.log10() - 3.0) / (2e9f64.log10() - 3.0)) as f32;
        let at = |hz: f64| egui::pos2(r.left() + frac(hz) * r.width(), r.center().y);
        let p = at(10e6);
        h.event(egui::Event::PointerMoved(p));
        h.run_steps(1);
        h.event(egui::Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE });
        h.run_steps(1);
        h.event(egui::Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE });
        h.run_steps(2);
        let tuned = |posts: &[(String, serde_json::Value)]| posts.iter().filter(|(p, _)| p == "/api/sdr/inspect").filter_map(|(_, b)| b["freq_hz"].as_f64()).last();
        let hz = tuned(&drain(&h)).expect("a click tunes");
        assert!((hz / 10e6 - 1.0).abs() < 0.03, "clicked near 10 MHz, tuned {hz}");
        // A drag tunes once, where it is let go.
        h.drag_at(at(1e6));
        h.run_steps(1);
        h.event(egui::Event::PointerMoved(at(3e6)));
        h.run_steps(1);
        h.event(egui::Event::PointerMoved(at(7e6)));
        h.run_steps(1);
        h.drop_at(at(7e6));
        h.run_steps(2);
        let posts = drain(&h);
        let n = posts.iter().filter(|(p, _)| p == "/api/sdr/inspect").count();
        let hz = tuned(&posts).expect("a drag tunes");
        assert!(n == 1 && (hz / 7e6 - 1.0).abs() < 0.03, "dragged to 7 MHz: {n} tunes, last {hz}");
    }

    #[test]
    fn the_shade_strip_keeps_the_frequency_and_unrolls() {
        let mut h = harness();
        h.run_steps(2);
        let ctx = h.ctx.clone();
        h.state_mut().app.toggle_shade(&ctx);
        h.run_steps(3);
        assert!(h.state().app.shade.is_some());
        // The strip alone: no receiver bar, the meter and the keys.
        assert_eq!(h.query_all_by_label("RECEIVER A").count(), 0);
        h.get_by_label("MUTE");
        h.get_by_label("UNROLL").click();
        h.run_steps(2);
        assert!(h.state().app.shade.is_none());
        h.get_by_label("RECEIVER A");
    }

    #[test]
    fn a_decode_pings_the_display_once() {
        let mut h = harness();
        h.run_steps(2);
        let event = proto::DecodeEvent { protocol: "P25".into(), kind: "voice header".into(), summary: "group call".into(), valid: true, fields: Default::default() };
        h.state().wire.events.send(NetEvent::Decode { inspect_hz: 146.04e6, event, at: std::time::SystemTime::now() }).unwrap();
        h.run_steps(2);
        assert_eq!(h.state().app.pan.pings_live(), 1);
    }

    fn harness_with(prefs: Prefs, w: f32, ht: f32) -> Harness<'static, T> {
        let ctx = egui::Context::default();
        let mut app = App::build(&ctx, prefs, Player::silent(), Player::silent(), None);
        let (link, wire) = Link::wire();
        app.link = Some(link);
        let info = proto::ServerInfo { name: "usdr".into(), version: "test".into(), api: 2, features: vec!["filter".into(), "rx2".into()] };
        wire.events.send(NetEvent::Connected(Box::new(rsp_status()), Some(info))).unwrap();
        let mut h = Harness::builder().with_size(egui::vec2(w, ht)).build_ui_state(
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
    fn a_step_picked_on_80_m_does_not_follow_into_broadcast_fm() {
        // Sideband with a 1 kHz step chosen by hand, then back to WFM.
        let mut h = harness();
        h.state_mut().app.set_mode(Mode::Lsb);
        h.state_mut().app.prefs.step_hz = Some(1_000.0);
        // Another sideband shares the raster: the chosen step stays.
        h.state_mut().app.set_mode(Mode::Usb);
        assert_eq!(h.state().app.prefs.step_hz, Some(1_000.0));
        h.state_mut().app.set_mode(Mode::Wfm);
        assert_eq!(h.state().app.prefs.step_hz, None, "the step went back to the mode's own");
        assert_eq!(h.state().app.step_hz(), 100_000.0);
        // And when the mode is changed elsewhere (the page, another
        // desktop), the same.
        let mut st = rsp_status();
        st.mode = Mode::Lsb;
        h.state_mut().app.mode_pending = None;
        h.state().wire.events.send(NetEvent::Status(Box::new(st.clone()))).unwrap();
        h.run_steps(2);
        h.state_mut().app.prefs.step_hz = Some(100.0);
        st.mode = Mode::Wfm;
        h.state().wire.events.send(NetEvent::Status(Box::new(st))).unwrap();
        h.run_steps(2);
        assert_eq!(h.state().app.prefs.step_hz, None);
    }

    #[test]
    fn a_status_still_on_its_way_does_not_put_the_old_mode_back() {
        // A memory's tune is answered before its mode change is: that
        // status still says the old mode. It must not bring back the old
        // mode's raster, or a click in that moment lands off the station.
        let mut h = harness();
        let mut old = rsp_status();
        old.mode = Mode::Lsb;
        h.state().wire.events.send(NetEvent::Status(Box::new(old.clone()))).unwrap();
        h.run_steps(2);
        assert_eq!(h.state().app.step_hz(), 100.0);
        h.state_mut().app.set_mode(Mode::Wfm);
        h.state().wire.events.send(NetEvent::Status(Box::new(old.clone()))).unwrap();
        h.run_steps(2);
        assert_eq!(h.state().app.status.mode, Mode::Wfm);
        assert_eq!(h.state().app.step_hz(), 100_000.0);
        // The server's answer settles it, and a later change made
        // elsewhere is followed again.
        let mut new = old.clone();
        new.mode = Mode::Wfm;
        h.state().wire.events.send(NetEvent::Status(Box::new(new))).unwrap();
        h.run_steps(2);
        assert!(h.state().app.mode_pending.is_none());
        h.state().wire.events.send(NetEvent::Status(Box::new(old))).unwrap();
        h.run_steps(2);
        assert_eq!(h.state().app.status.mode, Mode::Lsb);
    }

    #[test]
    fn the_jog_keys_put_an_off_raster_receiver_back_on_a_station() {
        // In WFM, 50 kHz beside a station (where 80 m's steps or a typed
        // frequency left it): one press of a jog key lands on the raster.
        let mut h = harness();
        let mut st = rsp_status();
        st.mode = Mode::Wfm;
        st.freq_hz = 99.1e6;
        st.inspect_hz = 99.95e6;
        st.rate_hz = 8e6;
        st.min_freq_hz = 95.25e6;
        st.max_freq_hz = 102.95e6;
        h.state().wire.events.send(NetEvent::Status(Box::new(st))).unwrap();
        h.run_steps(3);
        drain(&h);
        h.get_by_label("‹").click();
        h.run_steps(2);
        let posts = drain(&h);
        assert!(posts.contains(&("/api/sdr/inspect".into(), json!({ "freq_hz": 99_900_000.0 }))), "{posts:?}");
    }

    #[test]
    fn recalling_a_memory_tunes_and_switches_mode() {
        let mut h = harness();
        // The default bank's eighth slot is past the row, in its menu.
        h.get_by_label("+2").click();
        h.run_steps(2);
        h.get_by_label("8  98.1000 MHz  FM BC").click();
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
        let range = proto::FilterRange { min_width_hz: 2_500.0, max_width_hz: 25_000.0, max_edge_hz: 12_500.0, symmetric: true, shaped: false };
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
    fn span_lock_holds_the_zoomed_window_on_the_server() {
        let mut h = harness();
        h.run_steps(2);
        drain(&h);
        // Off and the server following: nothing to say.
        h.run_steps(2);
        assert!(!drain(&h).iter().any(|(p, _)| p == "/api/sdr/zoom"));
        // On: the hold is pushed once, not every frame.
        h.state_mut().app.prefs.span_lock = true;
        h.run_steps(3);
        let holds: Vec<_> = drain(&h).into_iter().filter(|(p, _)| p == "/api/sdr/zoom").collect();
        assert_eq!(holds, vec![("/api/sdr/zoom".into(), json!({ "hold": true }))]);
        // Held and zoomed, "center here" pans the window, not the tuner.
        h.state_mut().app.status.zoom = 16.0;
        h.state_mut().app.status.zoom_hold = true;
        h.state_mut().app.center_view(146.6e6);
        let posts = drain(&h);
        assert!(posts.contains(&("/api/sdr/zoom".into(), json!({ "hold": true, "center_hz": 146.6e6 }))), "{posts:?}");
        assert!(!posts.iter().any(|(p, _)| p == "/api/sdr/tune"), "{posts:?}");
        // Off again: the window follows the receiver once more.
        h.state_mut().app.prefs.span_lock = false;
        h.run_steps(2);
        assert!(drain(&h).contains(&("/api/sdr/zoom".into(), json!({ "hold": false }))));
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
    fn the_filter_shape_and_essb_presets_post_to_the_server() {
        let mut h = harness_sized(1900.0);
        h.state_mut().app.server = Some(proto::ServerInfo { name: "usdr".into(), version: "test".into(), api: 2, features: vec!["filter".into(), "filter-shape".into(), "rx2".into()] });
        filtered(&mut h, Mode::Lsb, 2_500.0, (200.0, 5_000.0), (-3_900.0, -300.0), true);
        // ESSB on LSB: the same 50–4000 Hz, below the carrier.
        h.get_by_label("ESSB").click();
        h.run_steps(2);
        assert!(drain(&h).contains(&("/api/sdr/filter".into(), json!({ "low_hz": -4000.0, "high_hz": -50.0 }))));
        // The shape is chosen from the header's key.
        h.get_by_label("Filter shape NORMAL").click();
        h.run_steps(2);
        h.get_by_role_and_label(egui::accesskit::Role::RadioButton, "SOFT").click();
        h.run_steps(2);
        assert!(drain(&h).contains(&("/api/sdr/filter".into(), json!({ "shape": "soft" }))));
        assert_eq!(h.state().app.status.filter_shape, proto::FilterShape::Soft);
        // A server without shapes shows no key.
        h.state_mut().app.server = Some(proto::ServerInfo { name: "usdr".into(), version: "test".into(), api: 2, features: vec!["filter".into()] });
        h.run_steps(2);
        assert_eq!(h.query_all_by_label("Filter shape SOFT").count(), 0);
    }

    #[test]
    fn the_receiver_bar_follows_receiver_b() {
        let mut h = harness();
        let range = proto::FilterRange { min_width_hz: 2_000.0, max_width_hz: 20_000.0, max_edge_hz: 10_000.0, symmetric: true, shaped: false };
        h.state_mut().app.modes = vec![ModeCaps { mode: Mode::Am, filter: Some(range), bandwidth_hz: 10_000.0, ..Default::default() }];
        h.state_mut().app.status.rx2 = Some(proto::SubRx { freq_hz: 146.1e6, mode: Mode::Nfm, bandwidth_hz: 25_000.0, ..Default::default() });
        h.state_mut().app.active_b = true;
        h.run_steps(3);
        // B's modes are the listener's five; P25 is not among them. Mode
        // keys pick one, so they are radio buttons.
        let radio = egui::accesskit::Role::RadioButton;
        let role = egui::accesskit::Role::CheckBox;
        // (The decode log keeps its P25 key inside the VOICE family.)
        assert_eq!(h.query_all_by_role_and_label(radio, "P25").count(), 0);
        h.get_by_role_and_label(radio, "AM").click();
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
        // Slot 1 is "AIR" in the default bank.
        let before = h.get_by_label("AIR").rect();
        let list = vec![
            crate::prefs::Memory { name: "NOAA WX".into(), freq_hz: 162.475e6, mode: Mode::Nfm },
            crate::prefs::Memory { name: "A".into(), freq_hz: 1.0e6, mode: Mode::Am },
            crate::prefs::Memory { name: "A long name for a memory".into(), freq_hz: 146.52e6, mode: Mode::Nfm },
        ];
        h.state_mut().app.save_memories(list);
        h.run_steps(3);
        // Three slots of other lengths: slot 1 has not moved, and the one
        // free slot follows the three at a stored slot's pitch.
        assert_eq!(h.get_by_label("NOAA WX").rect(), before);
        let free = h.get_by_label("empty slot 4").rect();
        let third = h.get_by_label("A long name for a memory").rect();
        assert!(free.left() > third.right() && third.width() <= crate::bar::SLOT_W + 0.5);
        assert!(h.query_by_label("empty slot 5").is_none());
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
    fn the_flag_counts_the_stations_in_the_newest_slot() {
        let mut h = harness();
        h.state_mut().app.status.mode = Mode::Ft8;
        assert_eq!(h.state().app.hf_flag_text().as_deref(), Some("FT8 · listening"));
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_millis() as u64;
        let (old, new) = ((now / 15_000 - 1) * 15_000, (now / 15_000) * 15_000);
        // An older slot of three, then the newest: two stations, one heard twice.
        for (slot, de) in [(old, "W1AW"), (old, "K1ABC"), (old, "N0XYZ"), (new, "K1ABC"), (new, "W9XYZ"), (new, "K1ABC")] {
            let fields = [("slotMs", slot.to_string()), ("de", de.to_string()), ("df", "1000".into())].into_iter().map(|(k, v)| (k.to_string(), v)).collect();
            let ev = proto::DecodeEvent { protocol: "FT8".into(), kind: "standard".into(), summary: format!("CQ {de}"), valid: true, fields };
            h.state().wire.events.send(NetEvent::Decode { inspect_hz: 14.074e6, event: ev, at: SystemTime::now() }).unwrap();
        }
        h.run_steps(2);
        assert_eq!(h.state().app.hf_flag_text().as_deref(), Some("FT8 · 2 stations"));
        // Other modes keep the classifier's line.
        h.state_mut().app.status.mode = Mode::Nfm;
        assert_eq!(h.state().app.hf_flag_text(), None);
    }

    #[test]
    fn ft8_decodes_read_as_a_slot_log_in_their_family() {
        let mut h = harness();
        for (text, df, snr) in [("CQ K1ABC FN42", "1234", "-12"), ("K1ABC W9XYZ -11", "811", "-3")] {
            let fields = [("snr", snr), ("dt", "0.2"), ("df", df), ("slot", "193015"), ("slotMs", "1762284615000")].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            let ev = proto::DecodeEvent { protocol: "FT8".into(), kind: "standard".into(), summary: text.into(), valid: true, fields };
            h.state().wire.events.send(NetEvent::Decode { inspect_hz: 14.074e6, event: ev, at: SystemTime::now() }).unwrap();
        }
        h.state_mut().app.status.mode = Mode::Ft8;
        h.run_steps(2);
        // The mode chose the family; the log is the HF view, both in it.
        h.state_mut().app.decode_family = Some(crate::decoders::Family::HfData);
        h.run_steps(3);
        let (a, b) = (h.get_by_label("K1ABC W9XYZ -11").rect(), h.get_by_label("CQ K1ABC FN42").rect());
        assert!(b.top() >= a.bottom() - 1.0, "{a:?} {b:?}");
        assert!(h.query_by_label("2 heard").is_some());
        // Another family hides them.
        h.state_mut().app.decode_family = Some(crate::decoders::Family::Paging);
        h.run_steps(2);
        assert!(h.query_by_label("CQ K1ABC FN42").is_none());
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
            filter: Some(proto::FilterRange { min_width_hz: min, max_width_hz: max, max_edge_hz: if matches!(mode, Mode::Usb | Mode::Lsb) { max } else { max / 2.0 }, symmetric: !matches!(mode, Mode::Usb | Mode::Lsb), shaped: matches!(mode, Mode::Usb | Mode::Lsb | Mode::Ft8 | Mode::Ft4) }),
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
    fn the_passband_is_widened_by_the_wheel_and_slid_by_a_drag() {
        let mut h = harness_sized(1900.0);
        filtered(&mut h, Mode::Usb, 2_100.0, (200.0, 4_000.0), (300.0, 2_400.0), true);
        let r = h.get_by_label("WIDTH").rect();
        let filters = |posts: Vec<(String, serde_json::Value)>| posts.into_iter().filter(|(p, _)| p == "/api/sdr/filter").map(|(_, b)| (b["low_hz"].as_f64().unwrap_or(f64::NAN), b["high_hz"].as_f64().unwrap_or(f64::NAN))).collect::<Vec<_>>();
        // Wheel up over it: 100 Hz wider, the low edge where it was.
        let inside = egui::pos2(r.left() + 0.3 * r.width(), r.center().y);
        h.event(egui::Event::PointerMoved(inside));
        h.run_steps(1);
        h.event(egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Line, delta: egui::vec2(0.0, 1.0), phase: egui::TouchPhase::Move, modifiers: egui::Modifiers::NONE });
        h.run_steps(2);
        assert_eq!(filters(drain(&h)).last(), Some(&(300.0, 2_500.0)));
        // A drag on the band slides both edges together.
        h.drag_at(inside);
        h.run_steps(1);
        h.event(egui::Event::PointerMoved(inside + egui::vec2(30.0, 0.0)));
        h.run_steps(1);
        h.drop_at(inside + egui::vec2(30.0, 0.0));
        h.run_steps(2);
        let (l, hi) = *filters(drain(&h)).last().expect("the slide set the filter");
        // (The wheel left it 2.2k wide.)
        assert!(l > 300.0 && ((hi - l) - 2_200.0).abs() < 1.0, "slid to {l}..{hi}");
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
        let mut h = harness_sized(2560.0);
        filtered(&mut h, Mode::Usb, 2_500.0, (200.0, 4_000.0), (300.0, 2_400.0), true);
        h.get_by_label("2.5k").click();
        h.run_steps(2);
        assert!(drain(&h).contains(&("/api/sdr/filter".into(), json!({ "reset": true }))));
        h.get_by_label("2.7k").click();
        h.run_steps(2);
        assert!(drain(&h).contains(&("/api/sdr/filter".into(), json!({ "low_hz": 300.0, "high_hz": 3000.0 }))));
    }

    /// Renders the receiver bar at the window widths it has fits for, in
    /// a voice mode, an HF data mode and with B, as PNGs for a look by eye:
    /// `USDR_QA_DIR=<dir> cargo test -p usdr-desktop qa_renders_the_receiver_bar -- --ignored`.
    #[test]
    #[ignore]
    fn qa_renders_the_receiver_bar() {
        let Some(dir) = std::env::var_os("USDR_QA_DIR").map(std::path::PathBuf::from) else { return };
        let modes: Vec<proto::ModeCaps> = Mode::ALL
            .iter()
            .map(|&mode| {
                let sideband = matches!(mode, Mode::Usb | Mode::Lsb | Mode::Ft8 | Mode::Ft4);
                proto::ModeCaps {
                    mode,
                    bandwidth_hz: if sideband { if matches!(mode, Mode::Ft8 | Mode::Ft4) { 3_000.0 } else { 2_500.0 } } else { 12_500.0 },
                    filter: matches!(mode, Mode::Usb | Mode::Lsb | Mode::Ft8 | Mode::Ft4 | Mode::Nfm | Mode::Am).then(|| proto::FilterRange { min_width_hz: 200.0, max_width_hz: if sideband { 5_000.0 } else { 25_000.0 }, max_edge_hz: if sideband { 5_000.0 } else { 12_500.0 }, symmetric: !sideband, shaped: sideband }),
                    description: format!("{} description", mode.label()),
                    ..Default::default()
                }
            })
            .collect();
        for w in [1100.0, 1280.0, 1400.0, 1600.0, 1900.0, 2560.0] {
            for (name, mode, lo, hi, b) in [("usb", Mode::Usb, 50.0, 4_000.0, false), ("ft8", Mode::Ft8, 100.0, 3_100.0, false), ("nfm", Mode::Nfm, -6_250.0, 6_250.0, false), ("b", Mode::Usb, 300.0, 2_800.0, true)] {
                let mut h = harness_wh(w, 400.0);
                {
                    let app = &mut h.state_mut().app;
                    app.server = Some(proto::ServerInfo { name: "usdr".into(), version: "test".into(), api: 2, features: vec!["filter".into(), "filter-shape".into(), "rx2".into()] });
                    app.modes = modes.clone();
                    app.status.mode = mode;
                    app.status.filter_low_hz = lo;
                    app.status.filter_high_hz = hi;
                    app.status.bandwidth_hz = hi - lo;
                    app.status.passband_offset_hz = (lo + hi) / 2.0;
                    app.status.filter_custom = true;
                    app.status.filter_shape = proto::FilterShape::Soft;
                    (app.status.tune_min_hz, app.status.tune_max_hz) = (1e3, 2e9);
                    app.pan.center_hz = app.status.inspect_hz;
                    app.pan.span_hz = 2e6;
                    if b {
                        app.status.rx2 = Some(proto::SubRx { freq_hz: 14.2e6, mode: Mode::Usb, bandwidth_hz: 2_500.0, passband_offset_hz: 1_550.0, ..Default::default() });
                        app.active_b = true;
                    }
                }
                h.run_steps(6);
                let img = h.render().expect("render");
                let crop = image::imageops::crop_imm(&img, 0, 0, img.width(), ((if w < 1500.0 { 250.0 } else { 170.0 }) * img.height() as f32 / 400.0) as u32).to_image();
                crop.save(dir.join(format!("bar-{name}-{w}.png"))).expect("save");
                if name == "usb" && w == 1900.0 {
                    // The shape menu, open.
                    h.get_by_label("Filter shape SOFT").click();
                    h.run_steps(4);
                    h.render().expect("render").save(dir.join("shape-menu.png")).expect("save");
                }
            }
        }
    }

    /// A station list lights the frequency the receiver is on, not the one
    /// the span happens to be centered on: zoomed out, the receiver moves
    /// inside the span and the tuner stays put.
    #[test]
    fn the_station_list_follows_the_receiver_not_the_span() {
        let mut h = harness();
        h.run_steps(2);
        let app = &mut h.state_mut().app;
        app.status.mode = Mode::Wefax;
        app.status.freq_hz = 9_108_100.0;
        app.status.inspect_hz = 6_338_600.0;
        let (st, khz) = crate::stations::WEFAX.tuned(app).expect("on a station");
        assert_eq!((st.call, khz), ("NMF", 6_340.5));
        app.status.inspect_hz = 6_338_000.0;
        assert!(crate::stations::WEFAX.tuned(app).is_none());
    }

    /// The HF station directories and data views, rendered for a look by
    /// eye. `USDR_QA_DIR=<dir>`.
    #[test]
    #[ignore]
    fn qa_renders_the_hf_directory() {
        let Some(dir) = std::env::var_os("USDR_QA_DIR").map(std::path::PathBuf::from) else { return };
        let mut h = harness_wh(1600.0, 980.0);
        // The HF key's menu, open, the receiver on FT8.
        h.state_mut().app.status.mode = Mode::Ft8;
        h.state_mut().app.status.inspect_hz = 14_074_000.0;
        h.run_steps(3);
        h.get_by_label("DATA").click();
        h.run_steps(4);
        h.render().expect("render").save(dir.join("hf-menu.png")).expect("save");
        h.get_by_label("DATA").click();
        h.run_steps(2);
        {
            let app = &mut h.state_mut().app;
            app.status.mode = Mode::Wefax;
            app.status.freq_hz = 9_108_100.0;
            app.status.inspect_hz = 6_338_600.0;
            app.status.digital_cfg = Some(proto::DigitalCfg::default());
            app.fax.open = true;
        }
        h.run_steps(4);
        h.get_by_label("STATIONS").click();
        h.run_steps(4);
        h.render().expect("render").save(dir.join("fax-stations.png")).expect("save");
        h.state_mut().app.fax.open = false;
        h.get_by_label("STATIONS").click();
        h.run_steps(2);
        h.state_mut().app.fax.open = true;
        h.run_steps(3);
        h.get_by_label("SCHEDULE").click();
        h.run_steps(4);
        h.render().expect("render").save(dir.join("fax-schedule.png")).expect("save");
        h.get_by_label("SCHEDULE").click();
        h.run_steps(2);
        h.state_mut().app.fax.open = false;

        // Each mode's own view, with its station list open.
        // Just off each station, so the station menu reads STATIONS.
        for (mode, hz) in [(Mode::Navtex, 517_300.0), (Mode::Hfdl, 8_912_300.0), (Mode::Drm, 9_570_300.0), (Mode::Wwv, 10_000_300.0), (Mode::Dsc, 8_413_100.0), (Mode::Sitor, 8_415_100.0)] {
            {
                let app = &mut h.state_mut().app;
                app.status.mode = mode;
                app.status.inspect_hz = hz;
                app.status.digital = None;
                app.decode_family = Some(crate::decoders::Family::HfData);
                app.decode_chip = None;
                app.dock_tab = DockTab::Decodes;
            }
            h.run_steps(3);
            let name = mode.wire();
            h.render().expect("render").save(dir.join(format!("view-{name}.png"))).expect("save");
            h.get_by_label("STATIONS").click();
            h.run_steps(4);
            h.render().expect("render").save(dir.join(format!("stations-{name}.png"))).expect("save");
            h.get_by_label("STATIONS").click();
            h.run_steps(2);
        }

        // Radiofax in the dock, a picture arriving; and PSK31 with one signal
        // picked beside the rest.
        {
            let mut h = harness_with(Prefs { auto_connect: false, dock_height: 420.0, ..Default::default() }, 1600.0, 1100.0);
            h.run_steps(2);
            {
                let app = &mut h.state_mut().app;
                app.status.mode = Mode::Wefax;
                app.status.inspect_hz = 8_680_100.0;
                app.status.digital = Some(proto::DigitalReading { locked: true, snr_db: Some(14.0), fax_state: Some("receiving".into()), fax_image: 3, fax_lines: 400, fax_width: 1809, ..Default::default() });
                app.status.digital_cfg = Some(proto::DigitalCfg::default());
                app.decode_family = Some(crate::decoders::Family::HfData);
                app.dock_tab = DockTab::Decodes;
            }
            for i in 0..400u32 {
                let row: Vec<u8> = (0..1809u32).map(|c| ((((c / 40 + i / 40) % 2) * 200) + 30) as u8).collect();
                h.state().wire.events.send(NetEvent::FaxLine(proto::FaxLine { rx: 0, image: 3, line: i, pixels: row })).unwrap();
            }
            h.run_steps(4);
            h.render().expect("render").save(dir.join("dock-wefax.png")).expect("save");
            {
                let app = &mut h.state_mut().app;
                app.status.mode = Mode::Psk31;
                app.status.inspect_hz = 14_070_000.0;
                app.status.digital = Some(proto::DigitalReading {
                    locked: true,
                    channels: vec![proto::PskChannel { id: 1, audio_hz: 875.0, snr_db: 6.0, active: true }, proto::PskChannel { id: 2, audio_hz: 1746.0, snr_db: 0.0, active: true }],
                    ..Default::default()
                });
            }
            for (ch, hz, text) in [(2, "1746", "CQ de PE1FTV PE1FTV pse k"), (1, "875", "cq de F4JTM F4JTM pse k"), (2, "1746", "btu Robert KF3AA de PE1FTV k")] {
                let fields = [("channel", ch.to_string()), ("df", hz.to_string())].into_iter().map(|(k, v)| (k.to_string(), v)).collect();
                let ev = proto::DecodeEvent { protocol: "PSK31".into(), kind: "text".into(), summary: text.into(), valid: true, fields };
                h.state().wire.events.send(NetEvent::Decode { inspect_hz: 14_070_000.0, event: ev, at: std::time::SystemTime::now() }).unwrap();
            }
            h.ctx.data_mut(|d| d.insert_temp(egui::Id::new("psk-pick"), 2u32));
            h.run_steps(4);
            h.render().expect("render").save(dir.join("dock-psk-pick.png")).expect("save");
        }

        // The map, with what was heard on the air: `USDR_QA_VIEWS` (HFDL)
        // and `USDR_QA_FT8`.
        {
            let mut h = harness_with(Prefs { auto_connect: false, dock_height: 520.0, ..Default::default() }, 1600.0, 1150.0);
            h.run_steps(2);
            let st = proto::Station { callsign: "N0CALL".into(), grid: "FN31pr".into(), antenna: String::new(), report_spots: false };
            h.state().wire.events.send(NetEvent::Station(st, proto::ReporterStatus::default())).unwrap();
            let mut feed = |path: Option<std::path::PathBuf>, key: &str| {
                let Some(b) = path.and_then(|p| std::fs::read(p).ok()) else { return };
                let v: serde_json::Value = serde_json::from_slice(&b).expect("json");
                let list = if key.is_empty() { v.clone() } else { v[key].clone() };
                for m in list.as_array().into_iter().flatten() {
                    let ev: proto::DecodeEvent = serde_json::from_value(m["event"].clone()).expect("event");
                    h.state().wire.events.send(NetEvent::Decode { inspect_hz: 0.0, event: ev, at: std::time::SystemTime::now() }).unwrap();
                }
            };
            feed(std::env::var_os("USDR_QA_VIEWS").map(|d| std::path::PathBuf::from(d).join("view-hfdl.json")), "decodes");
            feed(std::env::var_os("USDR_QA_FT8").map(std::path::PathBuf::from), "");
            h.state_mut().app.dock_tab = DockTab::Map;
            h.run_steps(4);
            h.render().expect("render").save(dir.join("map.png")).expect("save");
        }

        // A mode's view as it was on the air: `USDR_QA_VIEWS=<dir>` holding
        // view-<mode>.json, the decodes and last reading a client saw.
        if let Some(vdir) = std::env::var_os("USDR_QA_VIEWS").map(std::path::PathBuf::from) {
            for (mode, hz) in [(Mode::Drm, 9_570_000.0), (Mode::Hfdl, 11_387_000.0), (Mode::Wwv, 10_000_000.0)] {
                let Ok(b) = std::fs::read(vdir.join(format!("view-{}.json", mode.wire()))) else { continue };
                let v: serde_json::Value = serde_json::from_slice(&b).expect("json");
                let mut h = harness_with(Prefs { auto_connect: false, dock_height: 420.0, ..Default::default() }, 1600.0, 1100.0);
                h.run_steps(2);
                {
                    let app = &mut h.state_mut().app;
                    app.status.mode = mode;
                    app.status.inspect_hz = hz;
                    app.status.freq_hz = hz;
                    app.status.digital = serde_json::from_value(v["digital"].clone()).ok();
                    app.decode_family = Some(crate::decoders::Family::HfData);
                    app.dock_tab = DockTab::Decodes;
                }
                for m in v["decodes"].as_array().into_iter().flatten() {
                    let ev: proto::DecodeEvent = serde_json::from_value(m["event"].clone()).expect("event");
                    h.state().wire.events.send(NetEvent::Decode { inspect_hz: hz, event: ev, at: std::time::SystemTime::now() }).unwrap();
                }
                h.run_steps(4);
                h.render().expect("render").save(dir.join(format!("live-{}.png", mode.wire()))).expect("save");
            }
        }

        // FT8 as it was heard: `USDR_QA_FT8=<json>`, the decode messages a
        // web client saw on /ws; drawn in the default dock and a tall one.
        if let Some(b) = std::env::var_os("USDR_QA_FT8").and_then(|p| std::fs::read(p).ok()) {
            let msgs: Vec<serde_json::Value> = serde_json::from_slice(&b).expect("json");
            for (name, dock, ht) in [("dock-ft8.png", 230.0, 980.0), ("dock-ft8-tall.png", 520.0, 1150.0)] {
                let mut h = harness_with(Prefs { auto_connect: false, dock_height: dock, ..Default::default() }, 1600.0, ht);
                h.run_steps(2);
                // `USDR_QA_CALL` stands in for the station's callsign, to
                // see the calls sent to it picked out.
                let call = std::env::var("USDR_QA_CALL").unwrap_or_else(|_| "N0CALL".into());
                let st = proto::Station { callsign: call, grid: "FN31pr".into(), antenna: String::new(), report_spots: false };
                h.state().wire.events.send(NetEvent::Station(st, proto::ReporterStatus::default())).unwrap();
                {
                    let app = &mut h.state_mut().app;
                    app.status.mode = Mode::Ft8;
                    app.status.freq_hz = 14_074_000.0;
                    app.status.inspect_hz = 14_074_000.0;
                    app.status.filter_low_hz = 100.0;
                    app.status.filter_high_hz = 3_100.0;
                    app.decode_family = Some(crate::decoders::Family::HfData);
                    app.dock_tab = DockTab::Decodes;
                }
                for m in &msgs {
                    let ev: proto::DecodeEvent = serde_json::from_value(m["event"].clone()).expect("event");
                    h.state().wire.events.send(NetEvent::Decode { inspect_hz: 14_074_000.0, event: ev, at: std::time::SystemTime::now() }).unwrap();
                }
                h.run_steps(4);
                h.render().expect("render").save(dir.join(name)).expect("save");
                // A CQ clicked: its station opens beside the log.
                let cq = h.state().app.decodes.iter().position(|d| d.ev.summary.starts_with("CQ "));
                h.state_mut().app.selected_decode = cq;
                h.run_steps(4);
                h.render().expect("render").save(dir.join(name.replace(".png", "-inspect.png"))).expect("save");
            }
        }
    }

    /// The window at a laptop's 1280×800 on FT8, a crowded slot in the
    /// scope and a signal under the slice flag, and the classifier tab, for
    /// a look by eye: `USDR_QA_DIR=<dir> cargo test -p usdr-desktop
    /// qa_renders_the_laptop_window -- --ignored`.
    #[test]
    #[ignore]
    fn qa_renders_the_laptop_window() {
        let Some(dir) = std::env::var_os("USDR_QA_DIR").map(std::path::PathBuf::from) else { return };
        let mut h = harness_wh(1280.0, 800.0);
        {
            let app = &mut h.state_mut().app;
            app.status.mode = Mode::Ft8;
            app.status.freq_hz = 7_074_000.0;
            app.status.inspect_hz = 7_074_000.0;
            app.status.bandwidth_hz = 3_000.0;
            (app.status.tune_min_hz, app.status.tune_max_hz) = (1e3, 2e9);
            app.dock_tab = DockTab::Decodes;
            app.decode_family = Some(crate::decoders::Family::HfData);
        }
        // A noise floor with the FT8 sub-band lit just above the dial.
        let n = 2048usize;
        let (center, rate) = (7_074_000.0, 48_000.0);
        let pwr: Vec<f32> = (0..n)
            .map(|i| {
                let hz = center - rate / 2.0 + i as f64 / n as f64 * rate - 7_074_000.0;
                let tone = (200.0..2_800.0).contains(&hz) && (i * 7919) % 11 < 4;
                if tone { -55.0 } else { -105.0 + ((i * 31) % 7) as f32 }
            })
            .collect();
        for (df, de) in [(1_180.0, "PA3MUB"), (1_215.0, "K8OD"), (1_240.0, "K5DCW"), (1_262.0, "W1AW"), (1_410.0, "JA1XYZ"), (1_430.0, "VK2ABC"), (1_980.0, "EA8TL"), (2_400.0, "DL1ABC")] {
            let mut fields = std::collections::BTreeMap::new();
            fields.insert("df".to_string(), format!("{df}"));
            fields.insert("de".to_string(), de.to_string());
            fields.insert("slotMs".to_string(), "1000".to_string());
            let ev = proto::DecodeEvent { protocol: "FT8".into(), kind: "cq".into(), summary: format!("CQ {de}"), valid: true, fields };
            h.state().wire.events.send(NetEvent::Decode { inspect_hz: 7_074_000.0, event: ev, at: std::time::SystemTime::now() }).unwrap();
        }
        for _ in 0..4 {
            // The audio: a tone at each decode, for the scope's spectrum.
            let scope: Vec<i16> = (0..4096)
                .map(|i| {
                    let t = i as f64 / 12_000.0;
                    let v: f64 = [1_205.0, 1_240.0, 1_265.0, 1_287.0, 1_435.0, 1_455.0, 2_005.0, 2_425.0].iter().map(|f| (std::f64::consts::TAU * f * t).sin()).sum();
                    (v * 2_000.0) as i16
                })
                .collect();
            h.state().wire.events.send(NetEvent::Fft(Box::new(FftFrame { center_hz: center, rate_hz: rate, inspect_hz: 7_074_000.0, pwr: pwr.clone(), scope, scope_rate_hz: 12_000.0, ..Default::default() }))).unwrap();
            h.run_steps(2);
        }
        h.render().expect("render").save(dir.join("laptop-ft8.png")).expect("save");
        h.state_mut().app.dock_tab = DockTab::Classifier;
        h.run_steps(4);
        h.render().expect("render").save(dir.join("laptop-classifier.png")).expect("save");
        // The LO's notch in a wider span: tuner at 7.0 MHz, LO 100 kHz up.
        {
            let app = &mut h.state_mut().app;
            app.status.freq_hz = 7_000_000.0;
            app.status.lo_offset = true;
            app.status.lo_offset_hz = 100_000.0;
        }
        let wide: Vec<f32> = (0..n).map(|i| -105.0 + ((i * 31) % 7) as f32).collect();
        for _ in 0..4 {
            h.state().wire.events.send(NetEvent::Fft(Box::new(FftFrame { center_hz: 7_074_000.0, rate_hz: 200_000.0, inspect_hz: 7_074_000.0, pwr: wide.clone(), ..Default::default() }))).unwrap();
            h.run_steps(2);
        }
        h.render().expect("render").save(dir.join("laptop-lo.png")).expect("save");
    }

    /// The side panel with a station's RDS and an RSP's gain staging, for a
    /// look by eye: `USDR_QA_DIR=<dir> cargo test -p usdr-desktop
    /// qa_renders_the_side_panel -- --ignored`.
    #[test]
    #[ignore]
    fn qa_renders_the_side_panel() {
        let Some(dir) = std::env::var_os("USDR_QA_DIR").map(std::path::PathBuf::from) else { return };
        let mut h = harness_wh(1600.0, 1500.0);
        {
            let app = &mut h.state_mut().app;
            app.status.mode = Mode::Wfm;
            app.status.driver = "sdrplay".into();
            app.status.lna_steps_db = vec![0.0, 6.0, 12.0, 18.0, 37.0, 42.0, 61.0];
            app.status.rfgr_db = Some(2.0);
            app.status.ifgr_db = Some(38.0);
            app.status.lna_auto = true;
            app.status.rds = Some(proto::Rds { pi: "1234".into(), pi_call: Some("WTST".into()), ps: Some("TEST FM".into()), ps_steady: true, pty: Some(10), pty_name: Some("Country".into()), rt: Some("Test RadioText: an artist and a song title, as a station sends it".into()), locked: true, pilot: true, bler: 0.04, ..Default::default() });
        }
        h.run_steps(6);
        h.render().expect("render").save(dir.join("side-panel.png")).expect("save");
    }

    /// The AIS workspace with test vessels around a station, for a look by
    /// eye: `USDR_QA_DIR=<dir> cargo test -p usdr-desktop qa_renders_ais -- --ignored`.
    #[test]
    #[ignore]
    fn qa_renders_ais() {
        let Some(dir) = std::env::var_os("USDR_QA_DIR").map(std::path::PathBuf::from) else { return };
        let mut h = harness_with(Prefs { auto_connect: false, dock_height: 460.0, ..Default::default() }, 1600.0, 1000.0);
        h.run_steps(2);
        let st = proto::Station { callsign: "N0CALL".into(), grid: "FM19qg".into(), antenna: String::new(), report_spots: false };
        h.state().wire.events.send(NetEvent::Station(st, proto::ReporterStatus::default())).unwrap();
        {
            let app = &mut h.state_mut().app;
            app.status.mode = Mode::Ais;
            app.status.freq_hz = 162.0e6;
            app.status.rate_hz = 2e6;
            app.dock_tab = DockTab::Decodes;
        }
        // (MMSI, type, lat, lon, sog, cog, name, kind)
        let test: &[(&str, &str, f64, f64, f32, f32, &str, &str)] = &[
            ("367000001", "1", 39.22, -76.55, 11.2, 145.0, "TEST CARGO ONE", "cargo"),
            ("367000002", "1", 39.26, -76.58, 0.0, 0.0, "TEST TANKER", "tanker"),
            ("367000003", "18", 39.31, -76.46, 6.4, 40.0, "TEST SLOOP", "sailing"),
            ("367000004", "1", 39.18, -76.43, 9.8, 330.0, "TEST TUG", "tug"),
            ("993660001", "21", 39.24, -76.50, 0.0, 0.0, "TEST LIGHT", ""),
            ("003660001", "4", 39.27, -76.57, 0.0, 0.0, "", ""),
        ];
        for (mmsi, t, lat, lon, sog, cog, name, kind) in test {
            let mut fields = std::collections::BTreeMap::new();
            for (k, v) in [("mmsi", mmsi.to_string()), ("type", t.to_string()), ("lat", format!("{lat}")), ("lon", format!("{lon}")), ("sog", format!("{sog}")), ("cog", format!("{cog}")), ("name", name.to_string()), ("shipKind", kind.to_string())] {
                if !v.is_empty() {
                    fields.insert(k.to_string(), v);
                }
            }
            let ev = proto::DecodeEvent { protocol: "AIS".into(), kind: "position report".into(), summary: format!("{name} · {sog} kn"), valid: true, fields };
            h.state().wire.events.send(NetEvent::Decode { inspect_hz: 161.975e6, event: ev, at: std::time::SystemTime::now() }).unwrap();
        }
        h.run_steps(4);
        h.render().expect("render").save(dir.join("ais.png")).expect("save");
        h.get_by_label("DATA").click();
        h.run_steps(4);
        h.render().expect("render").save(dir.join("data-menu.png")).expect("save");
    }

    /// Renders the HF windows and views to PNGs for a look by eye, with a
    /// station, a radiofax picture and PSK31 traffic fed in as the server
    /// would send them. `USDR_QA_DIR=<dir>`; `USDR_QA_FAX=<a .pgm>` draws a
    /// real picture instead of a test pattern.
    #[test]
    #[ignore]
    fn qa_renders_the_hf_windows() {
        let Some(dir) = std::env::var_os("USDR_QA_DIR").map(std::path::PathBuf::from) else { return };
        let mut h = harness_wh(1600.0, 980.0);
        let save = |h: &mut Harness<'static, T>, name: &str| {
            h.run_steps(4);
            h.render().expect("render").save(dir.join(name)).expect("save");
        };
        let st = proto::Station { callsign: "N0CALL".into(), grid: "FM19la".into(), antenna: "40 m end-fed wire".into(), report_spots: true };
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let rep = proto::ReporterStatus { state: proto::ReporterState::Waiting, spots_sent: 214, datagrams: 9, queued: 17, heard_hour: 48, last_sent: Some(now - 140), next_send: Some(now + 162), last_error: None, ..Default::default() };
        h.state().wire.events.send(NetEvent::Station(st, rep)).unwrap();
        h.state_mut().app.settings_open = true;
        save(&mut h, "settings-station.png");
        h.state_mut().app.settings_page = crate::settings::Page::Interface;
        save(&mut h, "settings-interface.png");
        h.state_mut().app.settings_open = false;

        // A radiofax picture arriving.
        {
            let app = &mut h.state_mut().app;
            app.status.mode = Mode::Wefax;
            app.status.freq_hz = 13_880_600.0;
            app.status.digital = Some(proto::DigitalReading { locked: true, snr_db: Some(14.0), fax_state: Some("receiving".into()), fax_image: 3, fax_lines: 641, fax_width: 1809, ..Default::default() });
            app.status.digital_cfg = Some(proto::DigitalCfg::default());
        }
        let (w, rows): (usize, Vec<Vec<u8>>) = match std::env::var_os("USDR_QA_FAX").and_then(|p| std::fs::read(p).ok()) {
            Some(b) => {
                let header_end = b.iter().position(|&c| c == b'\n').unwrap() + 1;
                let head = String::from_utf8_lossy(&b[..header_end]).to_string();
                let w: usize = head.split_whitespace().nth(1).unwrap().parse().unwrap();
                (w, b[header_end..].chunks(w).map(|r| r.to_vec()).collect())
            }
            None => (1809, (0..400).map(|r| (0..1809).map(|c| (((c / 40 + r / 40) % 2) * 200 + 30) as u8).collect()).collect()),
        };
        for (i, row) in rows.into_iter().enumerate() {
            h.state().wire.events.send(NetEvent::FaxLine(proto::FaxLine { rx: 0, image: 3, line: i as u32, pixels: row })).unwrap();
        }
        let _ = w;
        h.state_mut().app.fax.open = true;
        save(&mut h, "fax-viewer.png");
        if let Some(p) = h.state_mut().app.fax.pictures.back_mut() {
            p.shift = p.margin_shift().unwrap_or(0);
            eprintln!("margin at column {}", p.shift);
        }
        save(&mut h, "fax-aligned.png");
        h.state_mut().app.fax.open = false;

        // PSK31 traffic in the dock.
        {
            let app = &mut h.state_mut().app;
            app.status.mode = Mode::Psk31;
            app.status.digital = Some(proto::DigitalReading {
                locked: true,
                channels: vec![
                    proto::PskChannel { id: 1, audio_hz: 875.0, snr_db: 6.0, active: true },
                    proto::PskChannel { id: 8, audio_hz: 1004.0, snr_db: -10.0, active: true },
                    proto::PskChannel { id: 2, audio_hz: 1746.0, snr_db: 0.0, active: false },
                ],
                ..Default::default()
            });
            app.decode_family = Some(crate::decoders::Family::HfData);
            app.dock_tab = DockTab::Decodes;
        }
        for (ch, hz, text) in [(2, "1746", "CQ de PE1FTV PE1FTV pse k"), (1, "875", "cq de F4JTM F4JTM pse k"), (8, "1004", "QRZ de TA3/HB9FIH kn"), (2, "1746", "btu Robert KF3AA de PE1FTV k")] {
            let mut fields = std::collections::BTreeMap::new();
            fields.insert("channel".to_string(), ch.to_string());
            fields.insert("df".to_string(), hz.to_string());
            let ev = proto::DecodeEvent { protocol: "PSK31".into(), kind: "text".into(), summary: text.into(), valid: true, fields };
            h.state().wire.events.send(NetEvent::Decode { inspect_hz: 14_070_000.0, event: ev, at: std::time::SystemTime::now() }).unwrap();
        }
        save(&mut h, "dock-psk31.png");
    }
}
