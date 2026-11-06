//! The HF text and picture decoders on receiver A's sideband audio: WEFAX,
//! RTTY, NAVTEX and PSK31. Each takes the audio before the AGC, as FT8's
//! does, and what it decodes leaves as the same `decode` events every other
//! decoder sends, so the log, the history and the web page need nothing new.
//! Radiofax rows are the exception: they go as 0x06 binary frames to the
//! desktop clients, and the picture so far is kept here for a client that
//! opens its viewer mid-fax (`GET /api/sdr/fax`).

use crate::sdr::{SdrEvent, SdrMode, SdrStatus};
use scannerd_engine::DecodeEvent;
use scannerd_engine::dsc::{DscDecoder, DscEvent};
use scannerd_engine::drm::{DrmEvent, DrmReceiver};
use scannerd_engine::hfdl::{HfdlDecoder, HfdlEvent};
use scannerd_engine::stanag::{StanagDecoder, StanagEvent};
use scannerd_engine::pactor::{PactorDecoder, PactorEvent, PactorMode};
use scannerd_engine::timecode::{TimecodeDecoder, TimecodeEvent};
use scannerd_engine::sitor::{SitorDecoder, SitorEvent, SitorMode};
use scannerd_engine::navtex::{NavtexDecoder, NavtexEvent};
use scannerd_engine::psk::PskDecoder;
use scannerd_engine::rtty::{RttyConfig, RttyDecoder};
use scannerd_engine::wefax::{WefaxDecoder, WefaxEvent, WefaxState};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast::Sender;
use usdr_proto::{DigitalCfg, DigitalReading, Fact, PskChannel};

/// The settings, server-wide like the filters: one radio, one table.
static CONFIG: Mutex<Option<DigitalCfg>> = Mutex::new(None);

pub fn config() -> DigitalCfg {
    CONFIG.lock().ok().and_then(|g| g.clone()).unwrap_or_default()
}

pub fn set_config(c: DigitalCfg) {
    if let Ok(mut g) = CONFIG.lock() {
        *g = Some(c);
    }
}

/// Bring a requested setting into the range the decoders take.
pub fn sanitize(mut c: DigitalCfg) -> DigitalCfg {
    if !(c.rtty_baud.is_finite() && (20.0..=300.0).contains(&c.rtty_baud)) {
        c.rtty_baud = 45.45;
    }
    if !(c.rtty_shift_hz.is_finite() && (50.0..=1_000.0).contains(&c.rtty_shift_hz)) {
        c.rtty_shift_hz = 170.0;
    }
    if c.psk_baud != 62.5 {
        c.psk_baud = 31.25;
    }
    if ![60, 90, 120, 240].contains(&c.fax_lpm) {
        c.fax_lpm = 120;
    }
    if c.fax_ioc != 288 {
        c.fax_ioc = 576;
    }
    if !c.fax_slant_ppm.is_finite() {
        c.fax_slant_ppm = 0.0;
    }
    c.fax_slant_ppm = c.fax_slant_ppm.clamp(-500.0, 500.0);
    if !["auto", "a", "b"].contains(&c.sitor_mode.as_str()) {
        c.sitor_mode = "auto".into();
    }
    c.drm_service = c.drm_service.min(3);
    c
}

/// The picture the server is receiving, or received last.
#[derive(Default)]
pub struct FaxPicture {
    pub id: u32,
    pub width: usize,
    pub pixels: Vec<u8>,
}

pub static FAX: Mutex<FaxPicture> = Mutex::new(FaxPicture { id: 0, width: 0, pixels: Vec::new() });
/// Pictures started since the server started; a new decoder (a mode change,
/// a retune that rebuilds the chain) never reuses a number.
static NEXT_PICTURE: AtomicU32 = AtomicU32::new(1);
/// The most rows kept for `GET /api/sdr/fax`, past any real transmission.
const FAX_MAX_ROWS: usize = 4_000;

enum Kind {
    Wefax { dec: Box<WefaxDecoder>, picture: Option<u32>, rows: u32, started_hz: f64 },
    Rtty(Box<RttyDecoder>),
    Navtex(Box<NavtexDecoder>),
    Psk(Box<PskDecoder>),
    Dsc(Box<DscDecoder>),
    Sitor(Box<SitorDecoder>),
    Pactor(Box<PactorDecoder>),
    Wwv(Box<TimecodeDecoder>),
    Hfdl(Box<HfdlDecoder>),
    Stanag(Box<StanagDecoder>),
    Drm {
        rx: Box<DrmReceiver>,
        audio: Box<crate::drm_audio::DrmAudio>,
        /// What was last sent of each, so a repeat (the SDC and text are
        /// sent over and over) is not logged again.
        last_service: String,
        last_text: String,
        last_time: String,
        locked: bool,
    },
}

impl HfEvent for DrmEvent {
    fn kind(&self) -> &'static str {
        DrmEvent::kind(self)
    }
    fn summary(&self) -> String {
        DrmEvent::summary(self)
    }
    fn fields(&self) -> Vec<(&'static str, String)> {
        DrmEvent::fields(self)
    }
}

impl HfEvent for StanagEvent {
    fn kind(&self) -> &'static str {
        StanagEvent::kind(self)
    }
    fn summary(&self) -> String {
        StanagEvent::summary(self)
    }
    fn fields(&self) -> Vec<(&'static str, String)> {
        StanagEvent::fields(self)
    }
}

/// The last symbols for a constellation, rounded so the reading changes
/// (and is sent) only when they do.
fn points(c: &[num_complex::Complex32], n: usize) -> Vec<[f32; 2]> {
    let r = |v: f32| (v * 50.0).round() / 50.0;
    c.iter().rev().take(n).rev().map(|p| [r(p.re), r(p.im)]).collect()
}

impl HfEvent for HfdlEvent {
    fn kind(&self) -> &'static str {
        HfdlEvent::kind(self)
    }
    fn summary(&self) -> String {
        HfdlEvent::summary(self)
    }
    fn fields(&self) -> Vec<(&'static str, String)> {
        HfdlEvent::fields(self)
    }
}

impl HfEvent for TimecodeEvent {
    fn kind(&self) -> &'static str {
        TimecodeEvent::kind(self)
    }
    fn summary(&self) -> String {
        TimecodeEvent::summary(self)
    }
    fn fields(&self) -> Vec<(&'static str, String)> {
        TimecodeEvent::fields(self)
    }
}

impl HfEvent for PactorEvent {
    fn kind(&self) -> &'static str {
        PactorEvent::kind(self)
    }
    fn summary(&self) -> String {
        PactorEvent::summary(self)
    }
    fn fields(&self) -> Vec<(&'static str, String)> {
        PactorEvent::fields(self)
    }
}

fn sitor_mode(c: &DigitalCfg) -> SitorMode {
    match c.sitor_mode.as_str() {
        "a" => SitorMode::A,
        "b" => SitorMode::B,
        _ => SitorMode::Auto,
    }
}

impl HfEvent for SitorEvent {
    fn kind(&self) -> &'static str {
        SitorEvent::kind(self)
    }
    fn summary(&self) -> String {
        SitorEvent::summary(self)
    }
    fn fields(&self) -> Vec<(&'static str, String)> {
        SitorEvent::fields(self)
    }
}

/// What the newer decoders' events have in common: a kind, a line for the
/// log, and their fields. Each leaves as a `decode` event of `protocol`.
trait HfEvent {
    fn kind(&self) -> &'static str;
    fn summary(&self) -> String;
    fn fields(&self) -> Vec<(&'static str, String)>;
    /// Whether every check it has passed.
    fn valid(&self) -> bool {
        true
    }
}

impl HfEvent for DscEvent {
    fn kind(&self) -> &'static str {
        DscEvent::kind(self)
    }
    fn summary(&self) -> String {
        DscEvent::summary(self)
    }
    fn fields(&self) -> Vec<(&'static str, String)> {
        DscEvent::fields(self)
    }
    fn valid(&self) -> bool {
        let DscEvent::Call(c) = self;
        c.ecc_ok && c.bad_fields.is_empty() && c.complete
    }
}

fn hf_event(protocol: &str, e: &impl HfEvent) -> DecodeEvent {
    DecodeEvent { protocol: protocol.into(), kind: e.kind().into(), summary: e.summary(), valid: e.valid(), fields: e.fields().into_iter().map(|(k, v)| (k.to_string(), v)).collect() }
}

pub struct HfDecoder {
    kind: Kind,
    last_reading: Instant,
    /// The reading last sent; `None` until the first, which always goes so
    /// a new decoder's reading replaces the last mode's.
    reading: Option<DigitalReading>,
}

/// The amateur preset at amateur speed; above it, the commercial one, whose
/// machines (DWD's among them) do not unshift on a space.
fn rtty_config(c: &DigitalCfg) -> RttyConfig {
    let mut r = if c.rtty_baud < 47.0 { RttyConfig::amateur() } else { RttyConfig::dwd() };
    r.baud = c.rtty_baud;
    r.shift_hz = c.rtty_shift_hz;
    r.reverse = c.rtty_reverse;
    r
}

fn event(protocol: &str, kind: &str, summary: String, fields: BTreeMap<String, String>) -> DecodeEvent {
    DecodeEvent { protocol: protocol.into(), kind: kind.into(), summary, valid: true, fields }
}

fn fields<const N: usize>(kv: [(&str, String); N]) -> BTreeMap<String, String> {
    kv.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

impl HfDecoder {
    pub fn new(mode: SdrMode, fs: f64) -> Option<Self> {
        let c = config();
        let fs = fs as f32;
        let kind = match mode {
            SdrMode::Wefax => {
                let mut d = WefaxDecoder::new(fs);
                d.set_lpm(c.fax_lpm as f32);
                d.set_ioc(c.fax_ioc);
                d.set_auto_start(c.fax_auto_start);
                d.set_slant_ppm(c.fax_slant_ppm as f64);
                Kind::Wefax { dec: Box::new(d), picture: None, rows: 0, started_hz: 0.0 }
            }
            SdrMode::Rtty => Kind::Rtty(Box::new(RttyDecoder::new(fs, rtty_config(&c)))),
            SdrMode::Navtex => Kind::Navtex(Box::new(NavtexDecoder::new(fs))),
            SdrMode::Psk31 => Kind::Psk(Box::new(PskDecoder::new(fs, c.psk_baud))),
            SdrMode::Dsc => Kind::Dsc(Box::new(DscDecoder::new(fs))),
            SdrMode::Pactor => Kind::Pactor(Box::new(PactorDecoder::new(fs))),
            SdrMode::Wwv => Kind::Wwv(Box::new(TimecodeDecoder::new(fs))),
            SdrMode::Hfdl => Kind::Hfdl(Box::new(HfdlDecoder::new(fs))),
            SdrMode::Stanag => Kind::Stanag(Box::new(StanagDecoder::new(fs))),
            SdrMode::Drm => Kind::Drm {
                rx: Box::new(DrmReceiver::new(fs)),
                audio: Box::new(crate::drm_audio::DrmAudio::new(fs as f64)),
                last_service: String::new(),
                last_text: String::new(),
                last_time: String::new(),
                locked: false,
            },
            SdrMode::Sitor => {
                let mut d = SitorDecoder::new(fs);
                d.set_mode(sitor_mode(&c));
                Kind::Sitor(Box::new(d))
            }
            _ => return None,
        };
        Some(HfDecoder { kind, last_reading: Instant::now() - Duration::from_secs(1), reading: None })
    }

    /// New settings, applied without rebuilding: a decoder keeps its lock.
    pub fn apply(&mut self, c: &DigitalCfg, fs: f64) {
        match &mut self.kind {
            Kind::Wefax { dec, .. } => {
                dec.set_lpm(c.fax_lpm as f32);
                dec.set_ioc(c.fax_ioc);
                dec.set_auto_start(c.fax_auto_start);
                dec.set_slant_ppm(c.fax_slant_ppm as f64);
            }
            Kind::Rtty(d) => d.set_config(rtty_config(c)),
            Kind::Psk(d) => {
                // The baud is the decoder's whole timing; a new one is a new
                // decoder.
                **d = PskDecoder::new(fs as f32, c.psk_baud);
            }
            Kind::Sitor(d) => d.set_mode(sitor_mode(c)),
            Kind::Navtex(_) | Kind::Dsc(_) | Kind::Pactor(_) | Kind::Wwv(_) | Kind::Hfdl(_) | Kind::Stanag(_) | Kind::Drm { .. } => {}
        }
    }

    pub fn fax(&mut self, start: bool, stop: bool) {
        if let Kind::Wefax { dec, .. } = &mut self.kind {
            if stop {
                dec.stop_now();
            }
            if start {
                dec.start_now();
            }
        }
    }

    /// Decode a block of channel I/Q (the decoders that read the carrier
    /// itself: WWV's time code, DRM), its first sample taken at `t0_ms` on
    /// the wall clock.
    pub fn push_iq(&mut self, iq: &[num_complex::Complex32], dial_hz: f64, t0_ms: u64, events: &Sender<SdrEvent>, status: &Arc<Mutex<SdrStatus>>) {
        let send = |ev: DecodeEvent| {
            let _ = events.send(SdrEvent::Decode { inspect_hz: dial_hz, event: ev });
        };
        match &mut self.kind {
            Kind::Wwv(d) => {
                // Stamped when the block reached the decoder, so the tick
                // offset includes the receive path's own delay.
                d.push_iq_at(iq, t0_ms as f64);
                for e in d.take() {
                    send(hf_event("WWV", &e));
                }
            }
            Kind::Drm { rx, audio, last_service, last_text, last_time, locked } => {
                rx.push_iq(iq);
                for e in rx.take() {
                    match &e {
                        // Every super frame: the audio waits for the codec,
                        // the numbers are in the reading.
                        DrmEvent::AudioSuperFrame { .. } | DrmEvent::Stats(_) => continue,
                        DrmEvent::Sync { locked: l, .. } => {
                            if *l == *locked {
                                continue;
                            }
                            *locked = *l;
                            if !*l {
                                audio.reset();
                            }
                        }
                        DrmEvent::Service(_) | DrmEvent::Text { .. } | DrmEvent::Time { .. } => {
                            let summary = e.summary();
                            let last: &mut String = match &e {
                                DrmEvent::Service(_) => &mut *last_service,
                                DrmEvent::Text { .. } => &mut *last_text,
                                _ => &mut *last_time,
                            };
                            if *last == summary {
                                continue;
                            }
                            *last = summary;
                        }
                    }
                    send(hf_event("DRM", &e));
                }
            }
            _ => {}
        }
        self.reading_tick(events, status);
    }

    /// For a decoder that makes the program audio itself (DRM): fill the
    /// block with it, silence until it has some. Others leave it alone.
    pub fn take_audio(&mut self, out: &mut Vec<f32>) {
        if let Kind::Drm { audio, .. } = &mut self.kind {
            let n = out.len();
            audio.take(n, out);
        }
    }

    /// Decode a block of audio and send what came of it. `dial_hz` is the
    /// suppressed carrier, which the log files each decode under.
    pub fn push(&mut self, audio: &[f32], dial_hz: f64, events: &Sender<SdrEvent>, status: &Arc<Mutex<SdrStatus>>) {
        let send = |ev: DecodeEvent| {
            let _ = events.send(SdrEvent::Decode { inspect_hz: dial_hz, event: ev });
        };
        match &mut self.kind {
            Kind::Wefax { dec, picture, rows, started_hz } => {
                dec.push(audio);
                for e in dec.take_events() {
                    match e {
                        WefaxEvent::Start { ioc, lpm } => {
                            let id = NEXT_PICTURE.fetch_add(1, Ordering::Relaxed);
                            *picture = Some(id);
                            *rows = 0;
                            *started_hz = dial_hz;
                            if let Ok(mut f) = FAX.lock() {
                                *f = FaxPicture { id, width: dec.width(), pixels: Vec::new() };
                            }
                            send(event(
                                "WEFAX",
                                "start",
                                format!("Radiofax picture begins · {lpm:.0} LPM · IOC {ioc}"),
                                fields([("image", id.to_string()), ("lpm", format!("{lpm:.0}")), ("ioc", ioc.to_string()), ("width", dec.width().to_string())]),
                            ));
                        }
                        WefaxEvent::Phased { .. } => {}
                        WefaxEvent::Line(px) => {
                            let Some(id) = *picture else { continue };
                            let frame = usdr_proto::encode_fax_line(0, id, *rows, &px);
                            let _ = events.send(SdrEvent::FaxRow { frame: frame.into() });
                            if let Ok(mut f) = FAX.lock()
                                && f.id == id
                                && f.width == px.len()
                                && f.pixels.len() < FAX_MAX_ROWS * f.width
                            {
                                f.pixels.extend_from_slice(&px);
                            }
                            *rows += 1;
                        }
                        WefaxEvent::Stop { .. } => {
                            if let Some(id) = picture.take() {
                                send(event(
                                    "WEFAX",
                                    "picture",
                                    format!("Radiofax picture · {} rows · {:.1} kHz", *rows, (*started_hz + 1_900.0) / 1e3),
                                    fields([("image", id.to_string()), ("lines", rows.to_string()), ("width", dec.width().to_string())]),
                                ));
                            }
                        }
                    }
                }
            }
            Kind::Rtty(d) => {
                d.push(audio);
                for t in d.take() {
                    if t.text.trim().is_empty() {
                        continue;
                    }
                    send(event(
                        "RTTY",
                        "text",
                        t.text,
                        fields([("df", format!("{:.0}", t.center_hz)), ("snr", format!("{:.0}", t.snr_db)), ("reverse", if t.reverse { "yes" } else { "no" }.into())]),
                    ));
                }
            }
            Kind::Navtex(d) => {
                d.push(audio);
                for e in d.take() {
                    match e {
                        NavtexEvent::Message(m) => {
                            let id = format!("{}{}{:02}", m.station, m.subject, m.serial);
                            send(event(
                                "NAVTEX",
                                if m.complete { "message" } else { "message (cut off)" },
                                m.text,
                                fields([
                                    ("id", id),
                                    ("station", m.station.to_string()),
                                    ("subject", m.subject.to_string()),
                                    ("subjectName", m.subject_name.to_string()),
                                    ("serial", m.serial.to_string()),
                                    ("complete", if m.complete { "yes" } else { "no" }.into()),
                                    ("errorRate", format!("{:.3}", m.error_rate)),
                                    ("df", format!("{:.0}", m.center_hz)),
                                    ("snr", format!("{:.0}", m.snr_db)),
                                ]),
                            ));
                        }
                        NavtexEvent::Text { text, in_message } => {
                            if text.trim().is_empty() {
                                continue;
                            }
                            send(event("NAVTEX", "text", text, fields([("inMessage", if in_message { "yes" } else { "no" }.into())])));
                        }
                    }
                }
            }
            Kind::Sitor(d) => {
                d.push(audio);
                for e in d.take() {
                    if matches!(&e, SitorEvent::Text { text, .. } if text.trim().is_empty()) {
                        continue;
                    }
                    send(hf_event("SITOR", &e));
                }
            }
            // Fed the channel I/Q (push_iq), not audio.
            Kind::Wwv(_) | Kind::Drm { .. } => {}
            Kind::Stanag(d) => {
                d.push(audio);
                for e in d.take() {
                    // ALE and the NATO modems both: the protocol names the
                    // one heard, so the log colors and files it.
                    let protocol = match &e {
                        StanagEvent::AleWord { .. } | StanagEvent::AleCall { .. } => "2G ALE",
                        _ => "STANAG",
                    };
                    send(hf_event(protocol, &e));
                }
            }
            Kind::Hfdl(d) => {
                d.push(audio);
                for e in d.take() {
                    send(hf_event("HFDL", &e));
                }
            }
            Kind::Pactor(d) => {
                d.push(audio);
                for e in d.take() {
                    if matches!(&e, PactorEvent::Text { text, .. } if text.trim().is_empty()) {
                        continue;
                    }
                    send(hf_event("PACTOR", &e));
                }
            }
            Kind::Dsc(d) => {
                d.push(audio);
                for e in d.take() {
                    send(hf_event("DSC", &e));
                }
            }
            Kind::Psk(d) => {
                d.push(audio);
                for t in d.take() {
                    if t.text.trim().is_empty() {
                        continue;
                    }
                    send(event(
                        "PSK31",
                        "text",
                        t.text,
                        fields([("channel", t.channel.to_string()), ("df", format!("{:.0}", t.audio_hz)), ("snr", format!("{:.0}", t.snr_db))]),
                    ));
                }
            }
        }
        self.reading_tick(events, status);
    }

    /// Send the reading, about once a second and only when it changed.
    fn reading_tick(&mut self, events: &Sender<SdrEvent>, status: &Arc<Mutex<SdrStatus>>) {
        if self.last_reading.elapsed() >= Duration::from_secs(1) {
            self.last_reading = Instant::now();
            let r = self.read();
            if self.reading.as_ref() != Some(&r) {
                self.reading = Some(r.clone());
                if let Ok(mut s) = status.lock() {
                    s.digital = Some(r);
                    let _ = events.send(SdrEvent::Status(s.clone()));
                }
            }
        }
    }

    fn read(&self) -> DigitalReading {
        let round = |v: f32| (v * 2.0).round() / 2.0;
        match &self.kind {
            Kind::Wefax { dec, picture, rows, .. } => {
                let state = match dec.state() {
                    WefaxState::Idle => "idle",
                    WefaxState::Starting => "starting",
                    WefaxState::Phasing => "phasing",
                    WefaxState::Receiving { .. } => "receiving",
                };
                DigitalReading {
                    locked: picture.is_some(),
                    snr_db: dec.carrier_snr_db().map(|v| v.round()),
                    fax_state: Some(state.into()),
                    fax_image: picture.unwrap_or(0),
                    fax_lines: *rows,
                    fax_width: dec.width() as u32,
                    ..Default::default()
                }
            }
            Kind::Rtty(d) => {
                let s = d.status();
                DigitalReading {
                    locked: s.locked,
                    center_hz: s.locked.then_some(s.center_hz.round()),
                    snr_db: s.locked.then_some(s.snr_db.round()),
                    reverse: s.locked.then_some(s.reverse),
                    ..Default::default()
                }
            }
            Kind::Navtex(d) => {
                let s = d.status();
                DigitalReading {
                    locked: s.locked,
                    center_hz: s.locked.then_some(s.center_hz.round()),
                    snr_db: s.locked.then_some(s.snr_db.round()),
                    in_message: s.in_message,
                    error_rate: s.locked.then_some((s.error_rate * 100.0).round() / 100.0),
                    ..Default::default()
                }
            }
            Kind::Sitor(d) => {
                let s = d.status();
                let mode = match s.mode {
                    SitorMode::A => Some("ARQ (A)"),
                    SitorMode::B if s.selective => Some("selective FEC (B)"),
                    SitorMode::B => Some("FEC (B)"),
                    SitorMode::Auto => None,
                };
                let mut facts = vec![Fact::new("link", s.link)];
                if let Some(side) = s.sending {
                    facts.push(Fact::new("sending", side.label()));
                }
                if let Some(c) = &s.selcall {
                    facts.push(Fact::new("selcall", c.clone()));
                }
                if let Some(id) = &s.identity {
                    facts.push(Fact::new("identity", id.clone()));
                }
                if s.locked {
                    facts.push(Fact::toned("errors", format!("{:.0}%", s.error_rate * 100.0), if s.error_rate > 0.1 { "warn" } else { "" }));
                }
                if s.forced {
                    facts.push(Fact::new("mode", "held"));
                }
                DigitalReading {
                    locked: s.locked,
                    center_hz: s.center_hz.map(f32::round),
                    snr_db: s.snr_db.map(f32::round),
                    reverse: s.locked.then_some(s.reverse),
                    error_rate: s.locked.then_some((s.error_rate * 100.0).round() / 100.0),
                    variant: mode.map(str::to_string),
                    facts,
                    ..Default::default()
                }
            }
            Kind::Pactor(d) => {
                let s = d.status();
                let variant = match (s.mode, s.baud) {
                    (Some(PactorMode::Unproto), Some(b)) => Some(format!("unproto · {b} baud")),
                    (Some(PactorMode::Arq), Some(b)) => Some(format!("ARQ · {b} baud{}", if s.longpath { " · long path" } else { "" })),
                    (_, Some(b)) => Some(format!("{b} baud")),
                    _ => None,
                };
                let mut facts = Vec::new();
                if let (Some(a), Some(b)) = (&s.caller, &s.called) {
                    facts.push(Fact::new("link", format!("{a} → {b}")));
                } else if let Some(b) = &s.called {
                    facts.push(Fact::new("calling", b.clone()));
                }
                if let Some(side) = s.sending {
                    facts.push(Fact::new("sending", side.as_str()));
                }
                if s.packets_ok + s.packets_memory_arq > 0 {
                    facts.push(Fact::new("packets", format!("{} ok · {} by memory ARQ", s.packets_ok, s.packets_memory_arq)));
                }
                DigitalReading { locked: s.locked, center_hz: s.center_hz.map(f32::round), snr_db: s.snr_db.map(f32::round), variant, facts, ..Default::default() }
            }
            Kind::Drm { rx, audio, .. } => {
                let s = rx.status();
                let variant = s.mode.map(|m| {
                    let mut v = format!("mode {m}");
                    if let Some(so) = s.spectrum_occupancy {
                        v += &format!(" · SO {so}");
                    }
                    if let Some(q) = s.msc_qam {
                        v += &format!(" · {q}-QAM");
                    }
                    v
                });
                let lamp = |on: bool| if on { "ok" } else { "" };
                let stages = [("time", s.time_locked), ("freq", s.freq_locked), ("frame", s.frame_locked), ("FAC", s.fac_locked), ("SDC", s.sdc_locked), ("MSC", s.msc_locked)];
                let mut facts: Vec<Fact> = stages.iter().map(|(n, on)| Fact::toned(*n, if *on { "●" } else { "○" }, lamp(*on))).collect();
                if let Some(m) = s.wmer_db {
                    facts.push(Fact::new("WMER", format!("{m:.1} dB")));
                }
                if s.time_locked {
                    facts.push(Fact::new("offset", format!("{:+.1} Hz", s.freq_offset_hz)));
                }
                if let Some(d) = s.delay_spread_ms {
                    facts.push(Fact::new("delay", format!("{d:.1} ms")));
                }
                if let Some(d) = s.doppler_hz {
                    facts.push(Fact::new("Doppler", format!("{d:.1} Hz")));
                }
                if s.msc_locked {
                    facts.push(Fact::new("audio", format!("{:.1} s buffered", audio.buffered_s())));
                }
                DigitalReading {
                    locked: s.fac_locked,
                    snr_db: s.snr_db.map(f32::round),
                    variant,
                    facts,
                    constellation: if s.frame_locked { points(&rx.constellation(), 200) } else { Vec::new() },
                    ..Default::default()
                }
            }
            Kind::Stanag(d) => {
                let s = d.status();
                let variant = s.waveform.map(|w| {
                    let mut v = w.to_string();
                    if let Some(m) = s.modulation {
                        v += &format!(" · {m}");
                    }
                    if let Some(r) = s.data_rate_bps {
                        v += &format!(" · {r} bps");
                    }
                    if let Some(i) = s.interleave {
                        v += &format!(" {i}");
                    }
                    v
                });
                let mut facts = Vec::new();
                if let Some(hz) = s.freq_error_hz {
                    facts.push(Fact::new("offset", format!("{hz:+.0} Hz")));
                }
                // How much the signal looks like each waveform, while
                // none is named.
                if s.waveform.is_none() {
                    for (name, score) in &s.scores {
                        if *score >= 0.2 {
                            facts.push(Fact::new(*name, format!("{:.0}%", score * 100.0)));
                        }
                    }
                }
                DigitalReading {
                    locked: s.locked,
                    center_hz: s.center_hz.map(f32::round),
                    snr_db: s.snr_db.map(f32::round),
                    variant,
                    facts,
                    constellation: if s.locked { points(&d.constellation(), 160) } else { Vec::new() },
                    ..Default::default()
                }
            }
            Kind::Hfdl(d) => {
                let s = d.status();
                let variant = s.rate_bps.map(|r| format!("{r} bps{}", match s.long_interleaver {
                    Some(true) => " · long",
                    Some(false) => " · short",
                    None => "",
                }));
                let mut facts = vec![Fact::new("bursts", format!("{} · {} decoded", s.bursts, s.decoded))];
                if s.crc_failed + s.lpdu_failed > 0 {
                    facts.push(Fact::new("failed", (s.crc_failed + s.lpdu_failed).to_string()));
                }
                if let Some(hz) = s.freq_error_hz {
                    facts.push(Fact::new("offset", format!("{hz:+.0} Hz")));
                }
                DigitalReading { locked: s.locked, center_hz: s.center_hz.map(f32::round), snr_db: s.snr_db.map(f32::round), variant, facts, ..Default::default() }
            }
            Kind::Wwv(d) => {
                let s = d.status();
                let heard: Vec<&str> = [("WWV", s.wwv_db), ("WWVH", s.wwvh_db)].iter().filter(|(_, db)| db.is_some()).map(|(n, _)| *n).collect();
                let mut facts = Vec::new();
                for (n, db) in [("WWV", s.wwv_db), ("WWVH", s.wwvh_db)] {
                    if let Some(db) = db {
                        facts.push(Fact::new(n, format!("{db:+.0} dB")));
                    }
                }
                if let Some(hz) = s.carrier_offset_hz {
                    facts.push(Fact::new("carrier", format!("{hz:+.1} Hz")));
                }
                if let Some(ms) = s.tick_offset_ms {
                    facts.push(Fact::new("ticks", format!("{ms:+.1} ms")));
                }
                if let Some(t) = &s.last_utc {
                    facts.push(Fact::toned("read", format!("{:02}:{:02} UTC", t.hour, t.minute), "ok"));
                }
                DigitalReading {
                    locked: s.locked,
                    snr_db: s.snr_db.map(f32::round),
                    variant: (!heard.is_empty()).then(|| heard.join(" + ")),
                    bits: s.bits.iter().map(|b| b.unwrap_or(3)).collect(),
                    facts,
                    ..Default::default()
                }
            }
            Kind::Dsc(d) => {
                let s = d.status();
                DigitalReading {
                    locked: s.locked,
                    center_hz: s.center_hz.map(f32::round),
                    snr_db: s.snr_db.map(f32::round),
                    in_message: s.receiving,
                    variant: s.receiving.then(|| "call arriving".into()),
                    facts: vec![Fact::new("calls", s.calls.to_string())],
                    ..Default::default()
                }
            }
            Kind::Psk(d) => {
                let mut channels: Vec<PskChannel> = d
                    .channels()
                    .into_iter()
                    .map(|c| PskChannel { id: c.id, audio_hz: round(c.audio_hz), snr_db: c.snr_db.round(), active: c.active })
                    .collect();
                channels.sort_by(|a, b| a.audio_hz.total_cmp(&b.audio_hz));
                DigitalReading { locked: channels.iter().any(|c| c.active), channels, ..Default::default() }
            }
        }
    }
}
