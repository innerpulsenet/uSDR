//! What the uSDR server says on the wire, for clients that are not the
//! browser page compiled into it.
//!
//! The server owns the formats: the binary FFT frame is
//! `sdr::encode_fft_frame`, the audio frame is written in `ws_client`, and the
//! JSON shapes are the serde derives on `SdrStatus`, `SdrEvent` and the REST
//! handlers in `main.rs`. Everything here is decoded tolerantly — a field the
//! server adds later is ignored, and one it drops comes back as its default —
//! so a client built against one server keeps working against the next.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Leading byte of a binary FFT frame. v4 is v3 with an optional waterfall
/// peak array after the classification block; a v3 reader stops before it.
pub const FFT_FRAME_MAGIC: u8 = 0x03;
/// Leading byte of a binary audio frame: `[0x01][u32 rate][i16 samples…]`.
pub const AUDIO_FRAME_MAGIC: u8 = 0x01;
/// Leading byte of a tagged audio frame from another receiver:
/// `[0x04][u8 receiver][u32 rate][i16 samples…]` (API 2, `rx2`).
pub const TAGGED_AUDIO_MAGIC: u8 = 0x04;

// ---------- binary frames ----------

/// One spectrum frame, about 25 a second.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FftFrame {
    pub center_hz: f64,
    pub rate_hz: f64,
    /// Strongest |I| or |Q| in the frame, 0..1. At 1.0 the ADC is clipping.
    pub peak_iq: f32,
    pub inspect_hz: f64,
    pub scope_rate_hz: f32,
    pub scope_kind: ScopeKind,
    pub symbol_rate_hz: f32,
    /// Power in the tuned channel and the noise floor beneath it.
    pub channel_dbfs: f32,
    pub noise_dbfs: f32,
    /// Demodulated audio or symbols for the scope, i16 at `scope_rate_hz`.
    pub scope: Vec<i16>,
    /// Welch-averaged power per bin, dB, across the displayed (zoomed) window.
    pub pwr: Vec<f32>,
    /// Slowly decaying max-hold trace.
    pub max_hold: Vec<f32>,
    /// Strongest segment per bin in this frame's interval, when sent (v4).
    pub wf_peak: Vec<f32>,
    /// The frame's own Welch mean per bin, before the display's bin
    /// smoothing and averaging, for the waterfall (v5; empty before).
    pub wf_row: Vec<f32>,
    pub peaks: Vec<Peak>,
    pub classification: Option<Classification>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScopeKind {
    #[default]
    Audio,
    Symbols,
    Mpx,
}

impl ScopeKind {
    fn from_byte(b: u8) -> Self {
        match b {
            1 => ScopeKind::Symbols,
            2 => ScopeKind::Mpx,
            _ => ScopeKind::Audio,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Peak {
    pub freq_hz: f64,
    pub snr_db: f32,
    /// Protocol last decoded at this frequency, if any.
    pub label: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Classification {
    pub active: bool,
    pub snr_db: f32,
    pub rf_dbfs: f32,
    pub peak_dev_hz: f32,
    pub rms_dev_hz: f32,
    pub center_offset_hz: f32,
    pub confidence: f32,
    pub protocol: String,
    pub modulation: String,
    pub details: Option<String>,
}

/// A block of demodulated audio.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioFrame {
    /// Which receiver: 0 the main one, 1 the second.
    pub rx: u8,
    pub rate_hz: u32,
    pub samples: Vec<i16>,
}

/// What a binary websocket message turned out to be.
#[derive(Clone, Debug, PartialEq)]
pub enum Binary {
    Fft(Box<FftFrame>),
    Audio(AudioFrame),
}

#[derive(Debug, PartialEq, Eq)]
pub struct DecodeError(pub &'static str);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for DecodeError {}

struct Reader<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self.at.checked_add(n).ok_or(DecodeError("length overflow"))?;
        let s = self.buf.get(self.at..end).ok_or(DecodeError("frame truncated"))?;
        self.at = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn f32(&mut self) -> Result<f32, DecodeError> {
        Ok(f32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn f64(&mut self) -> Result<f64, DecodeError> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn str8(&mut self) -> Result<String, DecodeError> {
        let n = self.u8()? as usize;
        Ok(String::from_utf8_lossy(self.take(n)?).into_owned())
    }
    /// A u32 count of fixed-size items, refused when it claims more than the
    /// frame holds, so a corrupt length cannot ask for gigabytes.
    fn count(&mut self, item: usize) -> Result<usize, DecodeError> {
        let n = self.u32()? as usize;
        if n.saturating_mul(item) > self.buf.len() - self.at.min(self.buf.len()) {
            return Err(DecodeError("count exceeds frame"));
        }
        Ok(n)
    }
    fn f32s(&mut self, n: usize) -> Result<Vec<f32>, DecodeError> {
        let raw = self.take(n * 4)?;
        Ok(raw
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
            .collect())
    }
    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.at)
    }
}

/// Decode one binary websocket message.
pub fn decode_binary(buf: &[u8]) -> Result<Binary, DecodeError> {
    match buf.first() {
        Some(&FFT_FRAME_MAGIC) => decode_fft(buf).map(|f| Binary::Fft(Box::new(f))),
        Some(&AUDIO_FRAME_MAGIC) => decode_audio(buf).map(Binary::Audio),
        Some(&TAGGED_AUDIO_MAGIC) if buf.len() >= 6 => {
            let mut a = decode_audio(&buf[1..])?;
            a.rx = buf[1];
            Ok(Binary::Audio(a))
        }
        Some(_) => Err(DecodeError("unknown frame type")),
        None => Err(DecodeError("empty frame")),
    }
}

pub fn decode_audio(buf: &[u8]) -> Result<AudioFrame, DecodeError> {
    let mut r = Reader { buf, at: 1 };
    let rate_hz = r.u32()?;
    let body = &buf[r.at..];
    let samples = body
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    Ok(AudioFrame { rx: 0, rate_hz, samples })
}

pub fn decode_fft(buf: &[u8]) -> Result<FftFrame, DecodeError> {
    let mut r = Reader { buf, at: 1 };
    let center_hz = r.f64()?;
    let rate_hz = r.f64()?;
    let peak_iq = r.f32()?;
    let inspect_hz = r.f64()?;
    let scope_rate_hz = r.u16()? as f32 * 10.0;
    let scope_kind = ScopeKind::from_byte(r.u8()?);
    let symbol_rate_hz = r.u16()? as f32 / 10.0;
    let channel_dbfs = r.f32()?;
    let noise_dbfs = r.f32()?;
    let n_scope = r.count(2)?;
    let scope = r
        .take(n_scope * 2)?
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    let n_pwr = r.count(4)?;
    let n_hold = r.count(4)?;
    let n_peaks = r.count(13)?;
    let pwr = r.f32s(n_pwr)?;
    let max_hold = r.f32s(n_hold)?;
    let mut peaks = Vec::with_capacity(n_peaks);
    for _ in 0..n_peaks {
        let freq_hz = r.f64()?;
        let snr_db = r.f32()?;
        let label = r.str8()?;
        peaks.push(Peak {
            freq_hz,
            snr_db,
            label: (!label.is_empty()).then_some(label),
        });
    }
    // v3: the classification block. Older frames may end before it.
    let classification = if r.remaining() > 0 && r.u8()? == 1 {
        let active = r.u8()? != 0;
        let snr_db = r.f32()?;
        let rf_dbfs = r.f32()?;
        let peak_dev_hz = r.f32()?;
        let rms_dev_hz = r.f32()?;
        let center_offset_hz = r.f32()?;
        let confidence = r.f32()?;
        let protocol = r.str8()?;
        let modulation = r.str8()?;
        let details = r.str8()?;
        Some(Classification {
            active,
            snr_db,
            rf_dbfs,
            peak_dev_hz,
            rms_dev_hz,
            center_offset_hz,
            confidence,
            protocol,
            modulation,
            details: (!details.is_empty()).then_some(details),
        })
    } else {
        None
    };
    // v4: the waterfall's per-frame peak array.
    let wf_peak = if r.remaining() >= 4 {
        let n = r.count(4)?;
        r.f32s(n)?
    } else {
        Vec::new()
    };
    // v5: the unsmoothed waterfall row.
    let wf_row = if r.remaining() >= 4 {
        let n = r.count(4)?;
        r.f32s(n)?
    } else {
        Vec::new()
    };
    Ok(FftFrame {
        center_hz,
        rate_hz,
        peak_iq,
        inspect_hz,
        scope_rate_hz,
        scope_kind,
        symbol_rate_hz,
        channel_dbfs,
        noise_dbfs,
        scope,
        pwr,
        max_hold,
        wf_peak,
        wf_row,
        peaks,
        classification,
    })
}

// ---------- JSON events ----------

/// The optional receive processing (`POST /api/sdr/processing`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Processing {
    /// "off", "low", "medium" or "high".
    pub nr: String,
    pub auto_notch: bool,
    /// 0 (off) to 3 (aggressive).
    pub blanker: u8,
    /// "fast", "medium" or "slow".
    pub agc: String,
}

/// The receiver's state, as `GET /api/sdr/status` and the `status` event
/// carry it. Field names are the server's (snake_case).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Status {
    pub serial: String,
    pub tuner: String,
    pub driver: String,
    pub host: Option<String>,
    pub gain_min_db: f64,
    pub gain_max_db: f64,
    pub rates_hz: Vec<f64>,
    pub tune_min_hz: f64,
    pub tune_max_hz: f64,
    pub bias_t: Option<bool>,
    pub rf_notch: Option<bool>,
    pub dab_notch: Option<bool>,
    pub agc_setpoint_dbfs: Option<f64>,
    pub iq_correction: Option<bool>,
    pub rfgr_db: Option<f64>,
    pub ifgr_db: Option<f64>,
    pub if_bandwidth_hz: Option<f64>,
    pub if_bandwidths_hz: Vec<f64>,
    pub if_bandwidth_manual: bool,
    pub freq_hz: f64,
    pub rate_hz: f64,
    pub gain_db: Option<f64>,
    pub gain_now_db: Option<f64>,
    pub ppm: f64,
    pub min_freq_hz: f64,
    pub max_freq_hz: f64,
    pub fft_size: usize,
    pub peak_iq: f32,
    pub inspect_hz: f64,
    pub inspect_rate_hz: f64,
    pub dropped_blocks: u64,
    pub lagged_blocks: u64,
    pub lost_samples: u64,
    pub gap_events: u64,
    pub error: Option<String>,
    pub mode: Mode,
    pub audio_rate_hz: f64,
    pub lo_offset: bool,
    pub lo_offset_hz: f64,
    pub clip_guard: bool,
    pub tuner_agc: bool,
    /// Noise reduction, auto-notch, blanker and AGC speed; absent from older
    /// servers.
    pub processing: Option<Processing>,
    pub zoom: f64,
    pub bandwidth_hz: f64,
    pub passband_offset_hz: f64,
    pub spurs_hz: Vec<f64>,
    pub freq_error_hz: Option<f64>,
    pub pager_sweep: Option<String>,
    pub pager_live_hz: Option<f64>,
    pub scan: Option<ScanStatus>,
    pub flex_diag: Option<serde_json::Value>,
    /// Display averaging ("off", "slow", "deep"); absent from older servers.
    pub avg: Option<String>,
    /// Passband edges in use, Hz from the carrier, and whether they are the
    /// operator's rather than the mode's own (API 2).
    pub filter_low_hz: f64,
    pub filter_high_hz: f64,
    pub filter_custom: bool,
    /// The second receiver, when one is listening (API 2, `rx2`).
    pub rx2: Option<SubRx>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubRx {
    pub freq_hz: f64,
    pub mode: Mode,
    pub bandwidth_hz: f64,
    pub passband_offset_hz: f64,
    /// Why it is silent, when it is.
    pub note: Option<String>,
}

/// Power in a channel and the floor under it, from a spectrum frame, the
/// way the server measures its own receiver: the channel's bins summed in
/// linear power, against the median bin scaled to the same width.
pub fn channel_level(pwr: &[f32], center_hz: f64, rate_hz: f64, channel_hz: f64, bandwidth_hz: f64) -> Option<(f32, f32)> {
    let n = pwr.len();
    if n < 8 || rate_hz <= 0.0 {
        return None;
    }
    let off = channel_hz - center_hz;
    let bin_of = |hz: f64| ((hz / rate_hz + 0.5) * (n - 1) as f64).round();
    let lo = bin_of(off - bandwidth_hz / 2.0);
    let hi = bin_of(off + bandwidth_hz / 2.0);
    if hi < 0.0 || lo > (n - 1) as f64 {
        return None;
    }
    let lo = lo.clamp(0.0, (n - 1) as f64) as usize;
    let hi = (hi.clamp(0.0, (n - 1) as f64) as usize).max(lo + 1).min(n - 1);
    let sum: f64 = pwr[lo..=hi].iter().map(|&db| 10f64.powf(f64::from(db) / 10.0)).sum();
    // The median, found without sorting the whole spectrum.
    let mut sorted = pwr.to_vec();
    let (_, median, _) = sorted.select_nth_unstable_by(n / 2, f32::total_cmp);
    let floor = 10f64.powf(f64::from(*median) / 10.0) * (hi - lo + 1) as f64;
    Some(((10.0 * sum.max(1e-30).log10()) as f32, (10.0 * floor.max(1e-30).log10()) as f32))
}

impl Status {
    pub fn is_sdrplay(&self) -> bool {
        self.driver.eq_ignore_ascii_case("sdrplay")
    }
    /// `RSP1A @ 172.16.0.216`, or the tuner alone for a local radio.
    pub fn radio_label(&self) -> String {
        match &self.host {
            Some(h) => format!("{} @ {h}", self.tuner),
            None => self.tuner.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Nfm,
    Am,
    Usb,
    Lsb,
    Wfm,
    P25,
    Dmr,
    Nxdn,
    Packet,
    Pager,
    Flex,
    Auto,
    Scan,
}

impl Mode {
    pub const ALL: [Mode; 13] = [
        Mode::Nfm,
        Mode::Am,
        Mode::Usb,
        Mode::Lsb,
        Mode::Wfm,
        Mode::P25,
        Mode::Dmr,
        Mode::Nxdn,
        Mode::Packet,
        Mode::Pager,
        Mode::Flex,
        Mode::Auto,
        Mode::Scan,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Mode::Nfm => "NFM",
            Mode::Am => "AM",
            Mode::Usb => "USB",
            Mode::Lsb => "LSB",
            Mode::Wfm => "WFM",
            Mode::P25 => "P25",
            Mode::Dmr => "DMR",
            Mode::Nxdn => "NXDN",
            Mode::Packet => "PACKET",
            Mode::Pager => "PAGER",
            Mode::Flex => "FLEX",
            Mode::Auto => "AUTO",
            Mode::Scan => "SCAN",
        }
    }
    pub fn wire(self) -> &'static str {
        match self {
            Mode::Nfm => "nfm",
            Mode::Am => "am",
            Mode::Usb => "usb",
            Mode::Lsb => "lsb",
            Mode::Wfm => "wfm",
            Mode::P25 => "p25",
            Mode::Dmr => "dmr",
            Mode::Nxdn => "nxdn",
            Mode::Packet => "packet",
            Mode::Pager => "pager",
            Mode::Flex => "flex",
            Mode::Auto => "auto",
            Mode::Scan => "scan",
        }
    }
}

/// One entry of `GET /api/sdr/modes`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModeCaps {
    pub mode: Mode,
    pub label: String,
    pub kind: String,
    pub description: String,
    pub delivers_audio: bool,
    pub delivers_decode_events: bool,
    pub bandwidth_hz: f32,
    pub scope_kind: ScopeKind,
    pub symbol_rate_hz: f32,
    pub passband_offset_hz: f32,
    pub capture: Vec<String>,
    /// How far the filter can be set; `None` where the decoder fixes it.
    pub filter: Option<FilterRange>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FilterRange {
    pub min_width_hz: f32,
    pub max_width_hz: f32,
    pub max_edge_hz: f32,
    pub symmetric: bool,
}

/// What the server says it is, from `GET /api/version` and the hello.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    /// API revision: 1 is the original page's API (no version route), 2
    /// adds presence, the web policy, shared memories and the filter.
    pub api: u32,
    pub features: Vec<String>,
}

/// The API revision this client is written for.
pub const API_REVISION: u32 = 2;

impl ServerInfo {
    pub fn has(&self, feature: &str) -> bool {
        self.features.iter().any(|f| f == feature)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanStatus {
    pub state: String,
    pub freq_hz: f64,
    pub progress: String,
    pub candidates: usize,
    pub testing: Vec<f64>,
    pub locked: Vec<f64>,
    pub hits: usize,
    pub skipped: usize,
    pub cfg: ScanCfg,
    pub message: Option<String>,
    pub skip_list: Vec<SkipEntry>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanCfg {
    pub start_hz: f64,
    pub stop_hz: f64,
    pub modes: Vec<String>,
    pub threshold_db: f32,
    pub resume_delay_s: f32,
    /// Fields this client does not edit ride along untouched, so posting the
    /// configuration back never resets them to defaults.
    #[serde(flatten)]
    pub other: BTreeMap<String, serde_json::Value>,
}

impl Default for ScanCfg {
    fn default() -> Self {
        Self {
            start_hz: 450e6,
            stop_hz: 470e6,
            modes: vec!["am".into(), "nfm".into(), "p25".into(), "dmr".into()],
            threshold_db: 10.0,
            resume_delay_s: 2.0,
            other: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SkipEntry {
    pub freq_hz: f64,
    pub reason: String,
    pub expires_in_s: u32,
    pub fails: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DecodeEvent {
    pub protocol: String,
    pub kind: String,
    pub summary: String,
    pub valid: bool,
    pub fields: BTreeMap<String, String>,
}

/// A recorded voice call from `GET /api/sdr/calls`. The metadata the panel
/// shows is typed; the rest is kept as-is for the diagnostics export.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Call {
    pub id: u64,
    pub protocol: String,
    pub started_ms: u64,
    pub duration_s: f32,
    pub freq_hz: f64,
    pub rate_hz: u32,
    pub samples: usize,
    pub iq_rate_hz: u32,
    pub iq_samples: usize,
    pub peak: f32,
    pub encrypted: bool,
    pub decrypted: bool,
    pub peak_snr_db: Option<f32>,
    pub freq_error_hz: Option<f32>,
    pub voiced_fraction: Option<f32>,
    pub algorithm: Option<String>,
    pub key_id: Option<u16>,
    pub tone: Option<String>,
    pub mdc: Option<String>,
    pub digital_protocol: Option<String>,
    pub color_code: Option<u8>,
    pub slot: Option<u8>,
    pub source_id: Option<u32>,
    pub target_id: Option<u32>,
    pub group: Option<bool>,
    pub manufacturer: Option<String>,
    pub emergency: bool,
    pub talker_alias: Option<String>,
    pub error_pct: Option<u8>,
    #[serde(flatten)]
    pub other: BTreeMap<String, serde_json::Value>,
}

/// One radio from `GET /api/devices`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Device {
    pub serial: String,
    pub label: String,
    pub tuner: String,
    pub host: Option<String>,
    /// `sdr` for the radio in use, `free` otherwise.
    pub role: String,
}

/// Who is connected to the server, and what the web page is allowed to do.
/// Sent by servers that know about desktop clients; older ones never send it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Presence {
    pub clients: Vec<ClientInfo>,
    pub web: WebPolicy,
}

impl Presence {
    pub fn desktop_count(&self) -> usize {
        self.clients.iter().filter(|c| c.kind == "desktop").count()
    }
    pub fn web_count(&self) -> usize {
        self.clients.iter().filter(|c| c.kind != "desktop").count()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientInfo {
    pub id: u64,
    /// `desktop` or `web`.
    pub kind: String,
    pub name: String,
    pub addr: String,
    /// Seconds since the epoch at connect.
    pub since: u64,
}

/// What the browser page may do while the server runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebPolicy {
    /// The page works alongside any desktop client; both follow the same
    /// receiver state.
    #[default]
    Shared,
    /// The page turns view-only while a desktop client is connected, and its
    /// changes are refused, so the desktop has the receiver to itself.
    Yield,
    /// The page is not served at all; the API stays up for desktop clients.
    Off,
}

impl WebPolicy {
    pub fn label(self) -> &'static str {
        match self {
            WebPolicy::Shared => "Shared",
            WebPolicy::Yield => "View-only while the app is connected",
            WebPolicy::Off => "Off",
        }
    }
}

/// A text websocket message.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The receiver's state on connect, and the server's description of
    /// itself (`None` from servers that predate API 2).
    Hello(Box<Status>, Option<ServerInfo>),
    Status(Box<Status>),
    Decode { inspect_hz: f64, event: DecodeEvent },
    Presence(Presence),
    Other(String),
}

pub fn decode_text(text: &str) -> Result<Event, serde_json::Error> {
    let v: serde_json::Value = serde_json::from_str(text)?;
    let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("").to_string();
    Ok(match kind.as_str() {
        "hello" => {
            let sdr = v.get("sdr").cloned().unwrap_or_default();
            let server = v.get("server").cloned().and_then(|s| serde_json::from_value(s).ok());
            Event::Hello(Box::new(serde_json::from_value(sdr)?), server)
        }
        "status" => Event::Status(Box::new(serde_json::from_value(v)?)),
        "decode" => Event::Decode {
            inspect_hz: v.get("inspectHz").and_then(|x| x.as_f64()).unwrap_or(0.0),
            event: serde_json::from_value(v.get("event").cloned().unwrap_or_default())?,
        },
        "presence" => Event::Presence(serde_json::from_value(v)?),
        _ => Event::Other(kind),
    })
}

/// Header a desktop client puts on its REST calls, so a server holding the
/// web page view-only can tell the desktop's changes from the page's.
pub const CLIENT_HEADER: &str = "x-usdr-client";

#[cfg(test)]
mod tests {
    use super::*;

    /// The layout `encode_fft_frame` writes, rebuilt by hand so the decoder
    /// is tested against the documented order rather than against itself.
    fn sample_frame(with_class: bool, with_wf: bool) -> Vec<u8> {
        let mut b = vec![FFT_FRAME_MAGIC];
        b.extend_from_slice(&104.1e6f64.to_le_bytes());
        b.extend_from_slice(&5e6f64.to_le_bytes());
        b.extend_from_slice(&0.25f32.to_le_bytes());
        b.extend_from_slice(&104.3e6f64.to_le_bytes());
        b.extend_from_slice(&4800u16.to_le_bytes()); // 48 kHz in 10 Hz steps
        b.push(1);
        b.extend_from_slice(&48000u16.to_le_bytes()); // 4800.0 baud ×10
        b.extend_from_slice(&(-40.5f32).to_le_bytes());
        b.extend_from_slice(&(-90.0f32).to_le_bytes());
        b.extend_from_slice(&3u32.to_le_bytes());
        for s in [1i16, -2, 3] {
            b.extend_from_slice(&s.to_le_bytes());
        }
        b.extend_from_slice(&4u32.to_le_bytes());
        b.extend_from_slice(&2u32.to_le_bytes());
        b.extend_from_slice(&1u32.to_le_bytes());
        for v in [-100.0f32, -90.0, -80.0, -70.0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        for v in [-60.0f32, -50.0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&104.2e6f64.to_le_bytes());
        b.extend_from_slice(&12.5f32.to_le_bytes());
        b.push(3);
        b.extend_from_slice(b"P25");
        if with_class {
            b.push(1);
            b.push(1);
            for v in [20.0f32, -30.0, 2800.0, 1500.0, -12.0, 0.9] {
                b.extend_from_slice(&v.to_le_bytes());
            }
            for s in ["P25 Phase 1", "C4FM", ""] {
                b.push(s.len() as u8);
                b.extend_from_slice(s.as_bytes());
            }
        } else {
            b.push(0);
        }
        if with_wf {
            b.extend_from_slice(&2u32.to_le_bytes());
            for v in [-10.0f32, -20.0] {
                b.extend_from_slice(&v.to_le_bytes());
            }
        }
        b
    }

    #[test]
    fn decodes_v4_frame() {
        let Binary::Fft(f) = decode_binary(&sample_frame(true, true)).unwrap() else {
            panic!("not an FFT frame")
        };
        assert_eq!(f.center_hz, 104.1e6);
        assert_eq!(f.scope_rate_hz, 48_000.0);
        assert_eq!(f.scope_kind, ScopeKind::Symbols);
        assert_eq!(f.symbol_rate_hz, 4800.0);
        assert_eq!(f.scope, vec![1, -2, 3]);
        assert_eq!(f.pwr.len(), 4);
        assert_eq!(f.max_hold, vec![-60.0, -50.0]);
        assert_eq!(f.peaks[0].label.as_deref(), Some("P25"));
        let c = f.classification.unwrap();
        assert_eq!(c.protocol, "P25 Phase 1");
        assert_eq!(c.details, None);
        assert_eq!(f.wf_peak, vec![-10.0, -20.0]);
    }

    #[test]
    fn decodes_v5_row_after_an_empty_peak_array() {
        let mut b = sample_frame(true, false);
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&4u32.to_le_bytes());
        for v in [-101.0f32, -91.0, -81.0, -71.0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        let f = decode_fft(&b).unwrap();
        assert!(f.wf_peak.is_empty());
        assert_eq!(f.wf_row, vec![-101.0, -91.0, -81.0, -71.0]);
        assert!(decode_fft(&sample_frame(true, true)).unwrap().wf_row.is_empty());
    }

    #[test]
    fn decodes_v3_frame_without_extras() {
        let f = decode_fft(&sample_frame(false, false)).unwrap();
        assert!(f.classification.is_none());
        assert!(f.wf_peak.is_empty());
    }

    #[test]
    fn refuses_truncated_and_lying_frames() {
        let full = sample_frame(true, true);
        assert!(decode_fft(&full[..40]).is_err());
        let mut lying = full.clone();
        // Claim a scope of a billion samples.
        lying[42..46].copy_from_slice(&1_000_000_000u32.to_le_bytes());
        assert!(decode_fft(&lying).is_err());
    }

    #[test]
    fn decodes_audio() {
        let mut b = vec![AUDIO_FRAME_MAGIC];
        b.extend_from_slice(&48_000u32.to_le_bytes());
        b.extend_from_slice(&(-5i16).to_le_bytes());
        b.extend_from_slice(&7i16.to_le_bytes());
        let Binary::Audio(a) = decode_binary(&b).unwrap() else { panic!() };
        assert_eq!(a.rate_hz, 48_000);
        assert_eq!(a.samples, vec![-5, 7]);
        assert_eq!(a.rx, 0);
        // The second receiver's frame: a tag byte and the receiver number.
        let mut t = vec![TAGGED_AUDIO_MAGIC, 1];
        t.extend_from_slice(&b[1..]);
        let Binary::Audio(a) = decode_binary(&t).unwrap() else { panic!() };
        assert_eq!((a.rx, a.rate_hz, a.samples.clone()), (1, 48_000, vec![-5, 7]));
    }

    #[test]
    fn decodes_status_and_decode_events() {
        let s = r#"{"type":"status","serial":"x","tuner":"RSP1A","freq_hz":1.0e8,"mode":"wfm","future_field":3}"#;
        let Event::Status(st) = decode_text(s).unwrap() else { panic!() };
        assert_eq!(st.mode, Mode::Wfm);
        assert_eq!(st.freq_hz, 1.0e8);
        let d = r#"{"type":"decode","inspectHz":1.5e8,"event":{"protocol":"POCSAG","kind":"page","summary":"hi","valid":true,"fields":{"addr":"123"}}}"#;
        let Event::Decode { inspect_hz, event } = decode_text(d).unwrap() else { panic!() };
        assert_eq!(inspect_hz, 1.5e8);
        assert_eq!(event.fields["addr"], "123");
        let h = r#"{"type":"hello","sdr":{"tuner":"R820T","mode":"nfm"}}"#;
        assert!(matches!(decode_text(h).unwrap(), Event::Hello(_, None)));
        let h2 = r#"{"type":"hello","sdr":{"tuner":"RSP1A"},"server":{"name":"usdr","version":"0.1.0","api":2,"features":["filter"]}}"#;
        let Event::Hello(_, Some(info)) = decode_text(h2).unwrap() else { panic!() };
        assert!(info.api == 2 && info.has("filter"));
    }

    #[test]
    fn scan_cfg_keeps_fields_it_does_not_know() {
        let j = r#"{"start_hz":1,"stop_hz":2,"modes":["am"],"threshold_db":9,"resume_delay_s":1,"slots":4}"#;
        let c: ScanCfg = serde_json::from_str(j).unwrap();
        let back = serde_json::to_value(&c).unwrap();
        assert_eq!(back["slots"], 4);
    }
}
