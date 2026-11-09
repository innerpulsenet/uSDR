//! The HF text and picture decoders on receiver A's sideband audio: WEFAX,
//! RTTY, NAVTEX, PSK31 and the rest. Each takes the audio before the AGC, as FT8's
//! does, and what it decodes leaves as the same `decode` events every other
//! decoder sends, so the log, the history and the web page need nothing new.
//! Radiofax rows are the exception: they go as 0x06 binary frames to the
//! desktop clients, and the picture so far is kept here for a client that
//! opens its viewer mid-fax (`GET /api/sdr/fax`).

use crate::sdr::{SdrEvent, SdrMode, SdrStatus};
use scannerd_engine::DecodeEvent;
use scannerd_engine::dsc::{DscDecoder, DscEvent};
use scannerd_engine::aac::{AacDecoder, DrmAacConfig, DrmAudioMode};
use scannerd_engine::drm::{AudioCodec, DrmEvent, DrmReceiver};
use scannerd_engine::cw::{CwDecoder, CwEvent};
use scannerd_engine::js8::{Js8Decoder, Js8Event, Js8Speed};
use scannerd_engine::sstv::{SstvDecoder, SstvEvent, SstvMode, SstvState};
use scannerd_engine::wspr::WsprDecoder;
use scannerd_engine::fst4w::Fst4wDecoder;
use scannerd_engine::hell::{HELL_ROWS, HellDecoder, HellEvent};
use scannerd_engine::mfsk::{MfskConfig, MfskDecoder, MfskEvent};
use scannerd_engine::olivia::{OliviaDecoder, OliviaEvent};
use scannerd_engine::ifk::{DominoExDecoder, IfkEvent, ThorDecoder};
use scannerd_engine::mt63::{Mt63Decoder, Mt63Event};
use scannerd_engine::selcal::{SelcalDecoder, SelcalEvent};
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
    if !SstvMode::ALL.iter().any(|m| m.name() == c.sstv_mode) {
        c.sstv_mode = SstvMode::ScottieS1.name().into();
    }
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

fn now_utc() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// An SSTV picture as its rows arrive. Unlike radiofax it is only streamed to
/// clients, so the daemon has to collect it to keep it.
#[derive(Default)]
struct SstvFrame {
    started_utc: u64,
    width: u32,
    height: u32,
    rgb: Vec<u8>,
}

impl SstvFrame {
    fn begin(width: u32, height: u32) -> Self {
        SstvFrame { started_utc: now_utc(), width, height, rgb: vec![0; width as usize * height as usize * 3] }
    }

    fn row(&mut self, line: u32, rgb: &[u8]) {
        let stride = self.width as usize * 3;
        let at = line as usize * stride;
        if rgb.len() == stride && at + stride <= self.rgb.len() {
            self.rgb[at..at + stride].copy_from_slice(rgb);
        }
    }

    /// Send the first `rows` rows on to be kept.
    fn hand_over(&mut self, rows: u32, dial_hz: f64) {
        let mut pixels = std::mem::take(&mut self.rgb);
        pixels.truncate(rows as usize * self.width as usize * 3);
        crate::pictures::submit(crate::pictures::Finished {
            kind: crate::pictures::Kind::Sstv,
            started_utc: self.started_utc,
            ended_utc: now_utc(),
            dial_hz: dial_hz.round() as u64,
            width: self.width,
            rows,
            full_rows: Some(self.height),
            pixels,
        });
    }
}

enum Kind {
    Wefax { dec: Box<WefaxDecoder>, picture: Option<u32>, rows: u32, started_hz: f64, started_utc: u64 },
    Rtty(Box<RttyDecoder>),
    Navtex(Box<NavtexDecoder>),
    Psk(Box<PskDecoder>),
    Dsc(Box<DscDecoder>),
    Sitor(Box<SitorDecoder>),
    Pactor(Box<PactorDecoder>),
    Wwv(Box<TimecodeDecoder>),
    Hfdl(Box<HfdlDecoder>),
    Stanag(Box<StanagDecoder>),
    Cw(Box<CwDecoder>),
    Wspr(Box<WsprDecoder>),
    Fst4w(Box<Fst4wDecoder>),
    Olivia(Box<OliviaDecoder>),
    Contestia(Box<OliviaDecoder>),
    Selcal(Box<SelcalDecoder>),
    Dominoex(Box<DominoExDecoder>),
    Thor(Box<ThorDecoder>),
    Mt63(Box<Mt63Decoder>),
    Mfsk(Box<MfskDecoder>),
    /// Feld Hell, and how many columns have gone out.
    Hell { dec: Box<HellDecoder>, sent: u32 },
    Js8(Box<Js8Decoder>),
    Sstv { dec: Box<SstvDecoder>, picture: Option<u32>, rows: u32, frame: SstvFrame },
    Drm {
        rx: Box<DrmReceiver>,
        audio: Box<crate::drm_audio::DrmAudio>,
        /// What was last sent of each, so a repeat (the SDC and text are
        /// sent over and over) is not logged again.
        last_service: String,
        last_text: String,
        last_time: String,
        locked: bool,
        /// The service whose audio plays (0 to 3), and the decoder for its
        /// codec, rebuilt when the service's audio configuration changes.
        service: u8,
        aac: Option<(DrmAacConfig, Box<AacDecoder>)>,
        frames_ok: u64,
        frames_lost: u64,
        /// Why there is no audio, when there is a service but none plays.
        no_audio: Option<&'static str>,
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

macro_rules! hf_event_impl {
    ($t:ty) => {
        impl HfEvent for $t {
            fn kind(&self) -> &'static str {
                <$t>::kind(self)
            }
            fn summary(&self) -> String {
                <$t>::summary(self)
            }
            fn fields(&self) -> Vec<(&'static str, String)> {
                <$t>::fields(self)
            }
        }
    };
}
hf_event_impl!(CwEvent);
hf_event_impl!(Js8Event);
hf_event_impl!(SstvEvent);
hf_event_impl!(OliviaEvent);
hf_event_impl!(SelcalEvent);
hf_event_impl!(IfkEvent);
hf_event_impl!(Mt63Event);
hf_event_impl!(MfskEvent);

/// The wall clock, ms since the Unix epoch: the slot decoders cut by it.
fn wall_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn sstv_mode(name: &str) -> SstvMode {
    SstvMode::ALL.into_iter().find(|m| m.name().eq_ignore_ascii_case(name)).unwrap_or(SstvMode::ScottieS1)
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

fn ifk_reading(s: scannerd_engine::ifk::IfkStatus) -> DigitalReading {
    DigitalReading {
        locked: s.locked,
        center_hz: Some(s.center_hz.round()),
        snr_db: s.snr_db.map(f32::round),
        variant: s.locked.then_some(s.variant),
        facts: vec![Fact::new("characters", s.chars.to_string())],
        ..Default::default()
    }
}

fn hf_event(protocol: &str, e: &impl HfEvent) -> DecodeEvent {
    DecodeEvent { protocol: protocol.into(), kind: e.kind().into(), summary: e.summary(), valid: e.valid(), fields: e.fields().into_iter().map(|(k, v)| (k.to_string(), v)).collect() }
}

pub struct HfDecoder {
    kind: Kind,
    /// The audio's sample rate.
    fs: f32,
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
                Kind::Wefax { dec: Box::new(d), picture: None, rows: 0, started_hz: 0.0, started_utc: 0 }
            }
            SdrMode::Rtty => Kind::Rtty(Box::new(RttyDecoder::new(fs, rtty_config(&c)))),
            SdrMode::Navtex => Kind::Navtex(Box::new(NavtexDecoder::new(fs))),
            SdrMode::Psk31 => Kind::Psk(Box::new(PskDecoder::new(fs, c.psk_baud))),
            SdrMode::Dsc => Kind::Dsc(Box::new(DscDecoder::new(fs))),
            SdrMode::Pactor => Kind::Pactor(Box::new(PactorDecoder::new(fs))),
            SdrMode::Wwv => Kind::Wwv(Box::new(TimecodeDecoder::new(fs))),
            SdrMode::Hfdl => Kind::Hfdl(Box::new(HfdlDecoder::new(fs))),
            SdrMode::Stanag | SdrMode::Ale => Kind::Stanag(Box::new(StanagDecoder::new(fs))),
            SdrMode::Cw => Kind::Cw(Box::new(CwDecoder::new(fs))),
            SdrMode::Wspr => Kind::Wspr(Box::new(WsprDecoder::new(fs))),
            SdrMode::Fst4w => Kind::Fst4w(Box::new(Fst4wDecoder::new(fs))),
            SdrMode::Olivia => Kind::Olivia(Box::new(OliviaDecoder::new_auto(fs, 1_500.0))),
            SdrMode::Contestia => Kind::Contestia(Box::new(OliviaDecoder::new_contestia_auto(fs, 1_500.0))),
            SdrMode::Dominoex => Kind::Dominoex(Box::new(DominoExDecoder::new_auto(fs, 1_500.0))),
            SdrMode::Thor => Kind::Thor(Box::new(ThorDecoder::new_auto(fs, 1_500.0))),
            SdrMode::Mt63 => Kind::Mt63(Box::new(Mt63Decoder::new_auto(fs, 1_500.0))),
            SdrMode::Selcal => Kind::Selcal(Box::new(SelcalDecoder::new(fs))),
            SdrMode::Mfsk => Kind::Mfsk(Box::new(MfskDecoder::new(fs, MfskConfig::default()))),
            SdrMode::Hell => Kind::Hell { dec: Box::new(HellDecoder::new(fs)), sent: 0 },
            SdrMode::Js8 => Kind::Js8(Box::new(Js8Decoder::new(fs, &Js8Speed::ALL))),
            SdrMode::Sstv => Kind::Sstv { dec: Box::new(SstvDecoder::new(fs)), picture: None, rows: 0, frame: SstvFrame::default() },
            SdrMode::Drm => Kind::Drm {
                rx: Box::new(DrmReceiver::new(fs)),
                audio: Box::new(crate::drm_audio::DrmAudio::new(fs as f64)),
                last_service: String::new(),
                last_text: String::new(),
                last_time: String::new(),
                locked: false,
                service: c.drm_service,
                aac: None,
                frames_ok: 0,
                frames_lost: 0,
                no_audio: None,
            },
            SdrMode::Sitor => {
                let mut d = SitorDecoder::new(fs);
                d.set_mode(sitor_mode(&c));
                Kind::Sitor(Box::new(d))
            }
            _ => return None,
        };
        Some(HfDecoder { kind, fs, last_reading: Instant::now() - Duration::from_secs(1), reading: None })
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
            Kind::Navtex(_) | Kind::Dsc(_) | Kind::Pactor(_) | Kind::Wwv(_) | Kind::Hfdl(_) | Kind::Stanag(_) | Kind::Cw(_) | Kind::Js8(_) | Kind::Sstv { .. } | Kind::Wspr(_) | Kind::Fst4w(_) | Kind::Olivia(_) | Kind::Contestia(_) | Kind::Selcal(_) | Kind::Dominoex(_) | Kind::Thor(_) | Kind::Mt63(_) | Kind::Mfsk(_) | Kind::Hell { .. } => {}
            Kind::Drm { service, audio, .. } => {
                if *service != c.drm_service {
                    *service = c.drm_service;
                    audio.reset();
                }
            }
        }
    }

    /// START and STOP for the picture modes: radiofax, and SSTV in the
    /// mode chosen for a picture whose VIS was missed.
    pub fn fax(&mut self, start: bool, stop: bool) {
        match &mut self.kind {
            Kind::Wefax { dec, .. } => {
                if stop {
                    dec.stop_now();
                }
                if start {
                    dec.start_now();
                }
            }
            Kind::Sstv { dec, .. } => {
                if stop {
                    dec.stop_now();
                }
                if start {
                    dec.start_now(sstv_mode(&config().sstv_mode));
                }
            }
            _ => {}
        }
    }

    /// The audio the multi-signal decoders search: the filter's passband.
    pub fn set_band(&mut self, low_hz: f32, high_hz: f32) {
        match &mut self.kind {
            Kind::Cw(d) => d.set_band(low_hz.max(150.0), high_hz.min(3_000.0)),
            Kind::Js8(d) => d.set_band(low_hz, high_hz),
            _ => {}
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
            Kind::Drm { rx, audio, last_service, last_text, last_time, locked, service, aac, frames_ok, frames_lost, no_audio } => {
                rx.push_iq(iq);
                for e in rx.take() {
                    match &e {
                        // Every super frame: its codec frames to the speaker,
                        // the numbers to the reading.
                        DrmEvent::AudioSuperFrame { service: sid, codec, sample_rate_hz, sbr, stereo, ps, frames, .. } => {
                            if *sid != *service {
                                continue;
                            }
                            if *codec != AudioCodec::Aac {
                                *no_audio = Some(match codec {
                                    AudioCodec::XheAac => "xHE-AAC is not decoded",
                                    _ => "this codec is not decoded",
                                });
                                continue;
                            }
                            let mode = if *ps { DrmAudioMode::ParametricStereo } else if *stereo { DrmAudioMode::Stereo } else { DrmAudioMode::Mono };
                            let cfg = DrmAacConfig { core_sample_rate_hz: *sample_rate_hz, sbr: *sbr, mode };
                            if aac.as_ref().is_none_or(|(c, _)| *c != cfg) {
                                *aac = Some((cfg, Box::new(AacDecoder::new_drm(cfg))));
                                audio.reset();
                            }
                            *no_audio = None;
                            let Some((_, dec)) = aac.as_mut() else { continue };
                            let channels = dec.output_channels().max(1) as usize;
                            let rate = dec.output_rate_hz() as f64;
                            for f in frames {
                                let pcm = match f.ok.then(|| dec.decode_drm(&f.data, f.crc.unwrap_or(0))) {
                                    Some(Ok(p)) => {
                                        *frames_ok += 1;
                                        p
                                    }
                                    // A frame that failed a check plays as the
                                    // decoder's concealment, never as noise.
                                    _ => {
                                        *frames_lost += 1;
                                        dec.conceal()
                                    }
                                };
                                // One channel to the listener: a stereo
                                // program's two, averaged.
                                let mono: Vec<f32> = if channels == 1 { pcm } else { pcm.chunks(channels).map(|c| c.iter().sum::<f32>() / channels as f32).collect() };
                                audio.push(&mono, rate);
                            }
                            continue;
                        }
                        DrmEvent::Stats(_) => continue,
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
            Kind::Wefax { dec, picture, rows, started_hz, started_utc } => {
                dec.push(audio);
                for e in dec.take_events() {
                    match e {
                        WefaxEvent::Start { ioc, lpm } => {
                            let id = NEXT_PICTURE.fetch_add(1, Ordering::Relaxed);
                            *picture = Some(id);
                            *rows = 0;
                            *started_hz = dial_hz;
                            *started_utc = now_utc();
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
                                let kept = FAX.lock().ok().filter(|f| f.id == id).map(|f| f.pixels.clone());
                                if let Some(pixels) = kept {
                                    let width = dec.width() as u32;
                                    crate::pictures::submit(crate::pictures::Finished {
                                        kind: crate::pictures::Kind::Wefax,
                                        started_utc: *started_utc,
                                        ended_utc: now_utc(),
                                        dial_hz: started_hz.round() as u64,
                                        width,
                                        rows: (pixels.len() / width.max(1) as usize) as u32,
                                        full_rows: None,
                                        pixels,
                                    });
                                }
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
            Kind::Cw(d) => {
                d.push(audio);
                for e in d.take() {
                    send(hf_event("CW", &e));
                }
            }
            Kind::Fst4w(d) => {
                let t0 = wall_ms().saturating_sub((audio.len() as f64 / self.fs as f64 * 1e3) as u64);
                d.push(audio, t0);
                for (slot_ms, decodes) in d.take() {
                    let secs = slot_ms / 1000;
                    for w in decodes {
                        let mut f = BTreeMap::new();
                        f.insert("tag".to_string(), w.tag.to_string());
                        f.insert("snr".to_string(), format!("{:.0}", w.snr_db));
                        f.insert("dt".to_string(), format!("{:.1}", w.dt_s));
                        f.insert("df".to_string(), format!("{:.0}", w.freq_hz));
                        f.insert("drift".to_string(), format!("{:.1}", w.drift_hz_per_min));
                        f.insert("call".to_string(), w.call.clone());
                        if let Some(g) = &w.grid {
                            f.insert("grid".to_string(), g.clone());
                        }
                        if let Some(p) = w.power_dbm {
                            f.insert("dbm".to_string(), p.to_string());
                        }
                        f.insert("slot".to_string(), format!("{:02}{:02}{:02}", (secs / 3600) % 24, (secs / 60) % 60, secs % 60));
                        f.insert("slotMs".to_string(), slot_ms.to_string());
                        send(DecodeEvent { protocol: "FST4W".into(), kind: w.kind.to_string(), summary: w.text.clone(), valid: true, fields: f });
                    }
                }
            }
            Kind::Wspr(d) => {
                // Stamped with its first sample: the block just ended.
                let t0 = wall_ms().saturating_sub((audio.len() as f64 / self.fs as f64 * 1e3) as u64);
                d.push(audio, t0);
                for (slot_ms, decodes) in d.take() {
                    let secs = slot_ms / 1000;
                    for w in decodes {
                        let mut f = BTreeMap::new();
                        f.insert("snr".to_string(), format!("{:.0}", w.snr_db));
                        f.insert("dt".to_string(), format!("{:.1}", w.dt_s));
                        f.insert("df".to_string(), format!("{:.0}", w.freq_hz));
                        f.insert("drift".to_string(), format!("{:.1}", w.drift_hz_per_min));
                        f.insert("call".to_string(), w.call.clone());
                        if let Some(g) = &w.grid {
                            f.insert("grid".to_string(), g.clone());
                        }
                        f.insert("dbm".to_string(), w.power_dbm.to_string());
                        f.insert("slot".to_string(), format!("{:02}{:02}{:02}", (secs / 3600) % 24, (secs / 60) % 60, secs % 60));
                        f.insert("slotMs".to_string(), slot_ms.to_string());
                        send(DecodeEvent { protocol: "WSPR".into(), kind: w.kind.into(), summary: w.text.clone(), valid: true, fields: f });
                    }
                }
            }
            Kind::Olivia(d) => {
                d.push(audio);
                for e in d.take() {
                    send(hf_event("OLIVIA", &e));
                }
            }
            Kind::Contestia(d) => {
                d.push(audio);
                for e in d.take() {
                    send(hf_event("CONTESTIA", &e));
                }
            }
            Kind::Dominoex(d) => {
                d.push(audio);
                for e in d.take() {
                    send(hf_event("DOMINOEX", &e));
                }
            }
            Kind::Thor(d) => {
                d.push(audio);
                for e in d.take() {
                    send(hf_event("THOR", &e));
                }
            }
            Kind::Mt63(d) => {
                d.push(audio);
                for e in d.take() {
                    send(hf_event("MT63", &e));
                }
            }
            Kind::Selcal(d) => {
                d.push(audio);
                for e in d.take() {
                    send(hf_event("SELCAL", &e));
                }
            }
            Kind::Mfsk(d) => {
                d.push(audio);
                for e in d.take() {
                    if let MfskEvent::Picture { width, height, color, pixels, .. } = &e {
                        // The picture to the desktop as color rows, as SSTV's.
                        let id = NEXT_PICTURE.fetch_add(1, Ordering::Relaxed);
                        let (w, per) = (*width as usize, if *color { 3 } else { 1 });
                        for (y, row) in pixels.chunks(w * per).take(*height as usize).enumerate() {
                            let rgb: Vec<u8> = if *color { row.to_vec() } else { row.iter().flat_map(|&g| [g, g, g]).collect() };
                            let _ = events.send(SdrEvent::FaxRow { frame: usdr_proto::encode_image_line(0, id, y as u32, &rgb).into() });
                        }
                        let mut ev = DecodeEvent { protocol: "MFSK".into(), kind: "picture".into(), summary: format!("MFSK picture {width}×{height}{}", if *color { " color" } else { "" }), valid: true, fields: BTreeMap::new() };
                        ev.fields.insert("image".into(), id.to_string());
                        send(ev);
                        continue;
                    }
                    send(hf_event("MFSK", &e));
                }
            }
            Kind::Hell { dec, sent } => {
                dec.push(audio);
                for e in dec.take() {
                    let HellEvent::Columns { pixels } = e;
                    let cols = (pixels.len() / HELL_ROWS) as u32;
                    if cols > 0 {
                        let _ = events.send(SdrEvent::FaxRow { frame: usdr_proto::encode_hell_columns(0, *sent, HELL_ROWS as u16, &pixels).into() });
                        *sent += cols;
                    }
                }
            }
            Kind::Js8(d) => {
                d.push(audio, wall_ms());
                for e in d.take() {
                    send(hf_event("JS8", &e));
                }
            }
            Kind::Sstv { dec, picture, rows, frame: collected } => {
                dec.push(audio);
                for e in dec.take() {
                    match &e {
                        SstvEvent::Start { width, height, .. } => {
                            *picture = Some(NEXT_PICTURE.fetch_add(1, Ordering::Relaxed));
                            *rows = 0;
                            *collected = SstvFrame::begin(*width, *height);
                            let mut ev = hf_event("SSTV", &e);
                            ev.fields.insert("image".into(), picture.unwrap_or(0).to_string());
                            send(ev);
                        }
                        SstvEvent::Line { line, rgb } => {
                            let Some(id) = *picture else { continue };
                            let frame = usdr_proto::encode_image_line(0, id, *line, rgb);
                            let _ = events.send(SdrEvent::FaxRow { frame: frame.into() });
                            *rows = (*rows).max(line + 1);
                            collected.row(*line, rgb);
                        }
                        SstvEvent::End { .. } => {
                            if picture.is_some() {
                                collected.hand_over(*rows, dial_hz);
                            }
                            let mut ev = hf_event("SSTV", &e);
                            if let Some(id) = picture.take() {
                                ev.fields.insert("image".into(), id.to_string());
                            }
                            send(ev);
                        }
                    }
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
                        fields([
                            ("channel", t.channel.to_string()),
                            ("df", format!("{:.0}", t.audio_hz)),
                            ("snr", format!("{:.0}", t.snr_db)),
                            ("mode", if config().psk_baud == 62.5 { "PSK63" } else { "PSK31" }.into()),
                        ]),
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
            Kind::Drm { rx, audio, frames_ok, frames_lost, no_audio, .. } => {
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
                if let Some(why) = no_audio {
                    facts.push(Fact::toned("audio", *why, "warn"));
                } else if s.msc_locked {
                    let total = frames_ok + frames_lost;
                    let lost = if total > 0 { 100.0 * *frames_lost as f64 / total as f64 } else { 0.0 };
                    facts.push(Fact::toned("audio", format!("{:.1} s buffered · {lost:.1}% concealed", audio.buffered_s()), if lost > 5.0 { "warn" } else { "" }));
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
            Kind::Cw(d) => {
                let s = d.status();
                let mut channels: Vec<PskChannel> = d.channels().into_iter().map(|c| PskChannel { id: c.id, audio_hz: round(c.audio_hz), snr_db: c.snr_db.round(), active: c.active }).collect();
                channels.sort_by(|a, b| a.audio_hz.total_cmp(&b.audio_hz));
                let variant = (s.active > 0).then(|| match s.wpm {
                    Some(w) => format!("{} signals · {w:.0} WPM", s.active),
                    None => format!("{} signals", s.active),
                });
                DigitalReading { locked: s.locked, center_hz: s.center_hz.map(f32::round), snr_db: s.snr_db.map(f32::round), variant, channels, ..Default::default() }
            }
            Kind::Dominoex(d) => ifk_reading(d.status()),
            Kind::Thor(d) => ifk_reading(d.status()),
            Kind::Mt63(d) => {
                let s = d.status();
                DigitalReading {
                    locked: s.locked,
                    center_hz: Some(s.center_hz.round()),
                    snr_db: s.snr_db.map(f32::round),
                    variant: s.locked.then_some(s.mode),
                    facts: vec![Fact::new("characters", s.chars.to_string())],
                    ..Default::default()
                }
            }
            Kind::Selcal(d) => {
                let s = d.status();
                let mut facts = vec![Fact::new("calls", s.calls.to_string())];
                if let Some(code) = s.last_code {
                    facts.push(Fact::new("last code", code));
                }
                DigitalReading { locked: s.locked, snr_db: s.snr_db.map(f32::round), variant: s.pair, facts, ..Default::default() }
            }
            Kind::Olivia(d) | Kind::Contestia(d) => {
                let s = d.status();
                DigitalReading {
                    locked: s.locked,
                    center_hz: Some(s.center_hz.round()),
                    snr_db: s.snr_db.map(f32::round),
                    variant: s.locked.then(|| format!("{}/{}", s.tones, s.bandwidth_hz)),
                    facts: vec![Fact::new("characters", s.chars.to_string())],
                    ..Default::default()
                }
            }
            Kind::Mfsk(d) => {
                let s = d.status();
                let mut facts = Vec::new();
                if s.locked {
                    facts.push(Fact::new("bit errors", format!("{:.0}%", s.bit_error_rate * 100.0)));
                }
                if let Some(ppm) = s.clock_ppm {
                    facts.push(Fact::new("clock", format!("{ppm:+.0} ppm")));
                }
                if let Some((w, h, color, done)) = s.picture {
                    facts.push(Fact::toned("picture", format!("{w}×{h}{} · {:.0}%", if color { " color" } else { "" }, done * 100.0), "ok"));
                }
                DigitalReading {
                    locked: s.locked,
                    center_hz: s.center_hz.map(f32::round),
                    snr_db: s.snr_db.map(f32::round),
                    variant: Some(s.mode.name().to_string()),
                    facts,
                    ..Default::default()
                }
            }
            Kind::Hell { dec, .. } => {
                let s = dec.status();
                DigitalReading { locked: s.locked, center_hz: s.center_hz.map(f32::round), snr_db: s.snr_db.map(f32::round), ..Default::default() }
            }
            Kind::Fst4w(d) => {
                let s = d.status();
                let mut facts = Vec::new();
                if let Some(t) = s.slot_secs {
                    facts.push(Fact::new("120 s slot", format!("{t:.0} s in")));
                }
                if let Some(n) = s.last_count {
                    facts.push(Fact::new("last slot", format!("{n} decoded")));
                }
                let variant = if s.decoding { Some("decoding".to_string()) } else if s.collecting { Some("collecting".to_string()) } else { None };
                DigitalReading { locked: s.collecting || s.decoding, variant, facts, ..Default::default() }
            }
            Kind::Wspr(d) => {
                let s = d.status();
                let mut facts = Vec::new();
                if let Some(t) = s.slot_secs {
                    facts.push(Fact::new("slot", format!("{t:.0} s of 120")));
                }
                if let Some(n) = s.last_count {
                    facts.push(Fact::new("last slot", format!("{n} decoded")));
                }
                let variant = if s.decoding { Some("decoding the slot".to_string()) } else if s.collecting { Some("collecting".to_string()) } else { None };
                DigitalReading { locked: s.collecting || s.decoding, variant, facts, ..Default::default() }
            }
            Kind::Js8(d) => {
                let s = d.status();
                let facts = vec![Fact::new("slots", s.slots_decoded.to_string()), Fact::new("frames", s.frames.to_string()), Fact::new("messages", s.messages.to_string())];
                DigitalReading { locked: s.frames > 0, facts, ..Default::default() }
            }
            Kind::Sstv { dec, picture, .. } => {
                let s = dec.status();
                let state = match s.state {
                    SstvState::Listening => "idle",
                    SstvState::Vis => "starting",
                    SstvState::Receiving => "receiving",
                };
                let mut facts = Vec::new();
                if matches!(s.state, SstvState::Receiving) {
                    facts.push(Fact::new("line", format!("{} of {}", s.line, s.lines)));
                    facts.push(Fact::new("sync", format!("{:.0}%", s.sync_quality * 100.0)));
                    facts.push(Fact::new("slant", format!("{:+.0} ppm", s.slant_ppm)));
                }
                DigitalReading {
                    locked: matches!(s.state, SstvState::Receiving),
                    variant: s.mode.map(|m| m.name().to_string()),
                    fax_state: Some(state.into()),
                    fax_image: picture.unwrap_or(0),
                    fax_lines: s.line,
                    facts,
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
