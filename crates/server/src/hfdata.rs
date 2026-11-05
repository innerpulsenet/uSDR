//! The HF text and picture decoders on receiver A's sideband audio: WEFAX,
//! RTTY, NAVTEX and PSK31. Each takes the audio before the AGC, as FT8's
//! does, and what it decodes leaves as the same `decode` events every other
//! decoder sends, so the log, the history and the web page need nothing new.
//! Radiofax rows are the exception: they go as 0x06 binary frames to the
//! desktop clients, and the picture so far is kept here for a client that
//! opens its viewer mid-fax (`GET /api/sdr/fax`).

use crate::sdr::{SdrEvent, SdrMode, SdrStatus};
use scannerd_engine::DecodeEvent;
use scannerd_engine::navtex::{NavtexDecoder, NavtexEvent};
use scannerd_engine::psk::PskDecoder;
use scannerd_engine::rtty::{RttyConfig, RttyDecoder};
use scannerd_engine::wefax::{WefaxDecoder, WefaxEvent, WefaxState};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast::Sender;
use usdr_proto::{DigitalCfg, DigitalReading, PskChannel};

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
            Kind::Navtex(_) => {}
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
        // The reading, about once a second and only when it changed.
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
