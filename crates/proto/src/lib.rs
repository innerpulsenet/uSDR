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

pub mod station;
pub use station::{ReporterState, ReporterStatus, Station, grid_center, normalize_callsign, normalize_grid};

/// Leading byte of a binary FFT frame. v4 is v3 with an optional waterfall
/// peak array after the classification block; a v3 reader stops before it.
pub const FFT_FRAME_MAGIC: u8 = 0x03;
/// Leading byte of a binary audio frame: `[0x01][u32 rate][i16 samples…]`.
pub const AUDIO_FRAME_MAGIC: u8 = 0x01;
/// Leading byte of a tagged audio frame from another receiver:
/// `[0x04][u8 receiver][u32 rate][i16 samples…]` (API 2, `rx2`).
pub const TAGGED_AUDIO_MAGIC: u8 = 0x04;
/// Leading byte of a broadcast FM audio frame with the stereo decoder's
/// reading, sent only to a client that connected with `audio=stereo` (and
/// only while the receiver is in WFM; every other mode, and every other
/// client, gets the mono frames above):
///
/// ```text
/// [0x05][u8 receiver][u8 channels][u8 flags][u8 blend][u8 snr_db]
///       [u16 pilot_dev_hz][u32 rate][i16 samples…]
/// ```
///
/// All little-endian. `channels` is 1 (mono) or 2, the samples then
/// interleaved L, R. `flags` bit 0: a pilot is locked at a pilot's level.
/// `blend` is how much of the stereo difference is in the samples, 0 (mono)
/// to 255 (full separation). `snr_db` is the S/N the decoder estimates the
/// stereo channels have, 255 when it has no estimate (no pilot).
/// `pilot_dev_hz` is the pilot's injection as deviation, 0 when there is no
/// pilot. Readers older than this skip it as an unknown frame, which is why
/// the server sends it only to clients that ask (API feature `stereo`).
pub const STEREO_AUDIO_MAGIC: u8 = 0x05;
/// Leading byte of one radiofax image row, sent only to desktop clients:
///
/// ```text
/// [0x06][u8 receiver][u32 image][u32 line][u16 width][u8 pixels…]
/// ```
///
/// All little-endian. `image` counts the pictures the server has started,
/// so a row from a new picture is never drawn into the last one. `line` is
/// the row's place in its picture, from 0. Each pixel is 0 (black) to 255
/// (white), `width` of them.
pub const FAX_LINE_MAGIC: u8 = 0x06;

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
    /// Mono, or interleaved L, R when `stereo` says two channels.
    pub samples: Vec<i16>,
    /// The stereo decoder's reading, on broadcast FM frames (0x05) only.
    pub stereo: Option<StereoReading>,
}

impl AudioFrame {
    pub fn channels(&self) -> usize {
        self.stereo.map_or(1, |s| s.channels.max(1) as usize)
    }
}

/// What the server's stereo decoder measured, carried with the audio it
/// shaped.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StereoReading {
    /// 1 or 2.
    pub channels: u8,
    /// A pilot is locked at a pilot's level.
    pub pilot: bool,
    /// Separation in use, 0 (mono) to 1.
    pub blend: f32,
    /// Estimated stereo S/N, dB, while a pilot is locked.
    pub snr_db: Option<f32>,
    /// The pilot's deviation, Hz, while one is locked.
    pub pilot_dev_hz: Option<f32>,
}

/// Write a 0x05 frame. The server writes the same bytes (its `ws_client`);
/// this is the reference, used by the tests on both sides.
pub fn encode_stereo_audio(rx: u8, rate_hz: u32, st: &StereoReading, samples: &[i16]) -> Vec<u8> {
    let mut b = Vec::with_capacity(12 + samples.len() * 2);
    b.push(STEREO_AUDIO_MAGIC);
    b.push(rx);
    b.push(st.channels);
    b.push(st.pilot as u8);
    b.push((st.blend.clamp(0.0, 1.0) * 255.0).round() as u8);
    b.push(st.snr_db.map_or(255, |d| d.round().clamp(0.0, 254.0) as u8));
    b.extend_from_slice(&st.pilot_dev_hz.map_or(0, |h| h.round().clamp(1.0, 65_535.0) as u16).to_le_bytes());
    b.extend_from_slice(&rate_hz.to_le_bytes());
    for s in samples {
        b.extend_from_slice(&s.to_le_bytes());
    }
    b
}

/// What a binary websocket message turned out to be.
#[derive(Clone, Debug, PartialEq)]
pub enum Binary {
    Fft(Box<FftFrame>),
    Audio(AudioFrame),
    FaxLine(FaxLine),
}

/// One radiofax row (see [`FAX_LINE_MAGIC`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FaxLine {
    pub rx: u8,
    pub image: u32,
    pub line: u32,
    pub pixels: Vec<u8>,
}

pub fn encode_fax_line(rx: u8, image: u32, line: u32, pixels: &[u8]) -> Vec<u8> {
    let width = pixels.len().min(u16::MAX as usize);
    let mut b = Vec::with_capacity(12 + width);
    b.push(FAX_LINE_MAGIC);
    b.push(rx);
    b.extend_from_slice(&image.to_le_bytes());
    b.extend_from_slice(&line.to_le_bytes());
    b.extend_from_slice(&(width as u16).to_le_bytes());
    b.extend_from_slice(&pixels[..width]);
    b
}

pub fn decode_fax_line(buf: &[u8]) -> Result<FaxLine, DecodeError> {
    let mut r = Reader { buf, at: 1 };
    let rx = r.u8()?;
    let image = r.u32()?;
    let line = r.u32()?;
    let width = r.u16()? as usize;
    let pixels = r.take(width)?.to_vec();
    Ok(FaxLine { rx, image, line, pixels })
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
        Some(&STEREO_AUDIO_MAGIC) => decode_stereo_audio(buf).map(Binary::Audio),
        Some(&FAX_LINE_MAGIC) => decode_fax_line(buf).map(Binary::FaxLine),
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
    Ok(AudioFrame { rx: 0, rate_hz, samples, stereo: None })
}

pub fn decode_stereo_audio(buf: &[u8]) -> Result<AudioFrame, DecodeError> {
    let mut r = Reader { buf, at: 1 };
    let rx = r.u8()?;
    let channels = r.u8()?;
    if !(1..=2).contains(&channels) {
        return Err(DecodeError("unsupported channel count"));
    }
    let flags = r.u8()?;
    let blend = r.u8()? as f32 / 255.0;
    let snr = r.u8()?;
    let pilot_hz = r.u16()?;
    let rate_hz = r.u32()?;
    let body = &buf[r.at..];
    if body.len() % (2 * channels as usize) != 0 {
        return Err(DecodeError("samples do not fill the channels"));
    }
    let samples = body.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
    Ok(AudioFrame {
        rx,
        rate_hz,
        samples,
        stereo: Some(StereoReading {
            channels,
            pilot: flags & 1 != 0,
            blend,
            snr_db: (snr != 255).then_some(snr as f32),
            pilot_dev_hz: (pilot_hz != 0).then_some(pilot_hz as f32),
        }),
    })
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
    /// IF gain reduction. Under the RSP's IF AGC it is the AGC's own
    /// report, and `None` from a driver that does not pass it on.
    pub ifgr_db: Option<f64>,
    /// SDRplay: the LNA reductions the band has, dB, one per state used,
    /// so `rfgr_db` (the state) can be read as decibels.
    pub lna_steps_db: Vec<f64>,
    /// The LNA is stepping on its own, following the RSP's IF AGC.
    pub lna_auto: bool,
    /// SDRplay: the API's power-overload report, now and how many times
    /// since the radio opened; `None` where the driver does not pass it on.
    pub overload: Option<bool>,
    pub overload_count: Option<u64>,
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
    /// Zoomed in, the window holds still while the receiver moves across it
    /// (span lock) rather than following it; absent from older servers.
    pub zoom_hold: bool,
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
    /// The passband skirt's shape (API 2, `filter-shape`); normal from
    /// older servers.
    pub filter_shape: FilterShape,
    /// The second receiver, when one is listening (API 2, `rx2`).
    pub rx2: Option<SubRx>,
    /// RDS from the broadcast station on receiver A, once its PI is
    /// confirmed (WFM only).
    pub rds: Option<Rds>,
    /// The stereo decoder on receiver A (WFM only).
    pub stereo: Option<Stereo>,
    /// How the HF text and picture decoders are set, and what the one
    /// running on receiver A hears (WEFAX, RTTY, NAVTEX, PSK31 only).
    pub digital_cfg: Option<DigitalCfg>,
    pub digital: Option<DigitalReading>,
}

/// The HF text and picture decoders' settings, server-wide and saved. Posted
/// whole or in part to `/api/sdr/digital`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DigitalCfg {
    pub rtty_baud: f32,
    pub rtty_shift_hz: f32,
    /// `None` finds the polarity from the signal; `Some(true)` is reversed
    /// (mark the lower tone).
    pub rtty_reverse: Option<bool>,
    pub psk_baud: f32,
    pub fax_lpm: u16,
    pub fax_ioc: u16,
    /// A picture begins on the station's start tone.
    pub fax_auto_start: bool,
    /// Line-period trim for a sample clock that runs fast or slow.
    pub fax_slant_ppm: f32,
    /// SITOR: `auto` tells ARQ (mode A) from FEC (mode B) by the timing;
    /// `a` or `b` holds one.
    pub sitor_mode: String,
    /// DRM: the service to play, 0 to 3.
    pub drm_service: u8,
}

impl Default for DigitalCfg {
    fn default() -> Self {
        Self {
            rtty_baud: 45.45,
            rtty_shift_hz: 170.0,
            rtty_reverse: None,
            psk_baud: 31.25,
            fax_lpm: 120,
            fax_ioc: 576,
            fax_auto_start: true,
            fax_slant_ppm: 0.0,
            sitor_mode: "auto".into(),
            drm_service: 0,
        }
    }
}

/// The running decoder's reading, about once a second.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DigitalReading {
    /// A signal is being copied.
    pub locked: bool,
    /// Audio frequency above the dial of the signal being copied.
    pub center_hz: Option<f32>,
    pub snr_db: Option<f32>,
    /// RTTY: the polarity found or forced.
    pub reverse: Option<bool>,
    /// NAVTEX: inside a message, between ZCZC and NNNN.
    pub in_message: bool,
    /// Characters received with an error, 0 to 1 (NAVTEX).
    pub error_rate: Option<f32>,
    /// WEFAX: `idle`, `starting`, `phasing` or `receiving`, the picture's
    /// number (as in the 0x06 rows), its rows so far and its width.
    pub fax_state: Option<String>,
    pub fax_image: u32,
    pub fax_lines: u32,
    pub fax_width: u32,
    /// PSK31: the signals being copied across the passband.
    pub channels: Vec<PskChannel>,
    /// What the decoder found the signal to be, in a few words: `ARQ (A)`,
    /// `STANAG 4285 · 8-PSK`, `B · SO 3 · 64-QAM`, `1200 bps`.
    pub variant: Option<String>,
    /// The decoder's other live numbers, as labeled values in the order to
    /// show them.
    pub facts: Vec<Fact>,
    /// WWV: the current minute's time code so far, a second each: 0 and 1
    /// for bits, 2 for a position marker, 3 for one not yet or not heard.
    pub bits: Vec<u8>,
    /// The last equalized symbols, for a constellation (STANAG, HFDL, DRM).
    pub constellation: Vec<[f32; 2]>,
}

/// One labeled number or word in a decoder's reading.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Fact {
    pub label: String,
    pub value: String,
    /// `ok`, `warn` or empty, for how it is drawn.
    pub tone: String,
}

impl Fact {
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Fact { label: label.into(), value: value.into(), tone: String::new() }
    }
    pub fn toned(label: impl Into<String>, value: impl Into<String>, tone: &str) -> Self {
        Fact { label: label.into(), value: value.into(), tone: tone.into() }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PskChannel {
    pub id: u32,
    pub audio_hz: f32,
    pub snr_db: f32,
    pub active: bool,
}

/// The stereo decoder's reading as the status carries it; the audio frames
/// carry the same, live, to a client that asked for stereo.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Stereo {
    pub pilot: bool,
    pub blend: f32,
    pub snr_db: Option<f32>,
    pub pilot_dev_hz: Option<f32>,
    /// The channel's multipath, its envelope's spread in percent, and
    /// whether the equalizer is undoing it.
    pub multipath_pct: Option<f32>,
    pub equalizer: bool,
}

/// What a broadcast FM station is sending over RDS. Every text here passed
/// its checkwords and was confirmed (see the engine's `rds` module for how).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rds {
    /// Program identification, four hex digits.
    pub pi: String,
    /// Call letters derived from the PI by the RBDS mapping.
    pub pi_call: Option<String>,
    /// The program service name (up to 8 characters).
    pub ps: Option<String>,
    /// The station scrolls words through its PS: `ps` is one word of a
    /// longer message, not the station's name.
    pub ps_dynamic: bool,
    /// The PS has held ten seconds and does not scroll: the station's name.
    pub ps_steady: bool,
    pub pty: Option<u8>,
    pub pty_name: Option<String>,
    pub tp: bool,
    pub ta: bool,
    pub rt: Option<String>,
    /// A RadioText still coming in, as far as it is confirmed, with U+2591
    /// (`░`) for each character not yet received; and how much is in, 0..1.
    pub rt_partial: Option<String>,
    pub rt_progress: f32,
    /// RT+ tags, while the station marks an item as running.
    pub title: Option<String>,
    pub artist: Option<String>,
    /// The station's clock as sent (`2026-11-03T19:32Z`) and its offset.
    pub clock_utc: Option<String>,
    pub clock_offset_min: Option<i32>,
    /// Block sync held now.
    pub locked: bool,
    /// The carrier is taken from the 19 kHz pilot.
    pub pilot: bool,
    /// Share of the last 50 blocks that could not be read, 0..1.
    pub bler: f32,
    pub updated_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubRx {
    pub freq_hz: f64,
    pub mode: Mode,
    pub bandwidth_hz: f64,
    pub passband_offset_hz: f64,
    /// Its mode's filter shape (`filter-shape`).
    pub filter_shape: FilterShape,
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
    /// HF weak-signal data: sideband audio cut into UTC slots and decoded,
    /// 15 s for FT8, 7.5 s for FT4.
    Ft8,
    Ft4,
    /// HF text and pictures on upper sideband: radiofax (weather charts),
    /// RTTY, NAVTEX and PSK31.
    Wefax,
    Rtty,
    Navtex,
    Psk31,
    /// HF utility and data links on upper sideband: SITOR A and B, MF/HF
    /// digital selective calling, the aeronautical HF data link, PACTOR-I,
    /// and the NATO serial-tone modems (STANAG 4285, MIL-STD-188-110).
    Sitor,
    Dsc,
    Hfdl,
    Pactor,
    Stanag,
    /// The NIST time stations' 100 Hz time code, on AM.
    Wwv,
    /// Digital Radio Mondiale: an OFDM broadcast decoded to its service
    /// information and audio.
    Drm,
    Auto,
    Scan,
}

impl Mode {
    pub const ALL: [Mode; 26] = [
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
        Mode::Ft8,
        Mode::Ft4,
        Mode::Wefax,
        Mode::Rtty,
        Mode::Navtex,
        Mode::Psk31,
        Mode::Sitor,
        Mode::Dsc,
        Mode::Hfdl,
        Mode::Pactor,
        Mode::Stanag,
        Mode::Wwv,
        Mode::Drm,
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
            Mode::Ft8 => "FT8",
            Mode::Ft4 => "FT4",
            Mode::Wefax => "WEFAX",
            Mode::Rtty => "RTTY",
            Mode::Navtex => "NAVTEX",
            Mode::Psk31 => "PSK31",
            Mode::Sitor => "SITOR",
            Mode::Dsc => "DSC",
            Mode::Hfdl => "HFDL",
            Mode::Pactor => "PACTOR",
            Mode::Stanag => "STANAG",
            Mode::Wwv => "WWV",
            Mode::Drm => "DRM",
            Mode::Auto => "AUTO",
            Mode::Scan => "SCAN",
        }
    }
    /// The HF data modes, behind the mode board's HF key: a decoder on the
    /// receiver's signal, its decodes in the HF data log.
    pub fn is_hf_data(self) -> bool {
        self.is_sideband_data() || matches!(self, Mode::Wwv | Mode::Drm)
    }

    /// The HF data modes heard on upper sideband: the tuned frequency is the
    /// dial (the suppressed carrier) and the decoder finds its signal in
    /// the audio above it.
    pub fn is_sideband_data(self) -> bool {
        matches!(self, Mode::Ft8 | Mode::Ft4 | Mode::Wefax | Mode::Rtty | Mode::Navtex | Mode::Psk31 | Mode::Sitor | Mode::Dsc | Mode::Hfdl | Mode::Pactor | Mode::Stanag)
    }

    /// Demodulated as single sideband: voice, and the sideband data modes.
    pub fn is_sideband(self) -> bool {
        matches!(self, Mode::Usb | Mode::Lsb) || self.is_sideband_data()
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
            Mode::Ft8 => "ft8",
            Mode::Ft4 => "ft4",
            Mode::Wefax => "wefax",
            Mode::Rtty => "rtty",
            Mode::Navtex => "navtex",
            Mode::Psk31 => "psk31",
            Mode::Sitor => "sitor",
            Mode::Dsc => "dsc",
            Mode::Hfdl => "hfdl",
            Mode::Pactor => "pactor",
            Mode::Stanag => "stanag",
            Mode::Wwv => "wwv",
            Mode::Drm => "drm",
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
    /// Whether the skirt's shape can be chosen (`filter-shape`).
    pub shaped: bool,
}

/// How a sideband filter's skirt falls away outside its edges: sharp
/// (elliptic, ~100 Hz to 70 dB, rings most), normal (elliptic, ~300 Hz to
/// 60 dB) or soft (linear phase, ~400 Hz, barely rings). A name this
/// client does not know reads as normal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FilterShape {
    Sharp,
    Soft,
    /// Last, as serde's catch-all must be.
    #[default]
    #[serde(other)]
    Normal,
}

impl FilterShape {
    pub const ALL: [FilterShape; 3] = [FilterShape::Sharp, FilterShape::Normal, FilterShape::Soft];

    pub fn wire(self) -> &'static str {
        match self {
            FilterShape::Sharp => "sharp",
            FilterShape::Normal => "normal",
            FilterShape::Soft => "soft",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FilterShape::Sharp => "SHARP",
            FilterShape::Normal => "NORMAL",
            FilterShape::Soft => "SOFT",
        }
    }

    /// What choosing it trades, for a tooltip.
    pub fn describe(self) -> &'static str {
        match self {
            FilterShape::Sharp => "Sharp: the skirt falls 70 dB within about 100 Hz of each edge. Most selective; rings most on clicks and crashes",
            FilterShape::Normal => "Normal: 60 dB down about 300 Hz past each edge. Keeps a neighbor out without a sharp filter's ring",
            FilterShape::Soft => "Soft: linear phase, rolling off over about 400 Hz. Least ringing, most natural speech; least selective",
        }
    }

    /// How far past each edge the skirt reaches the stopband, Hz.
    pub fn skirt_hz(self) -> f32 {
        match self {
            FilterShape::Sharp => 100.0,
            FilterShape::Normal => 300.0,
            FilterShape::Soft => 400.0,
        }
    }
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
    /// The station and the spot reporter's state (servers with `station`).
    Station(Station, ReporterStatus),
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
        "station" => Event::Station(
            serde_json::from_value(v.get("station").cloned().unwrap_or_default())?,
            serde_json::from_value(v.get("reporter").cloned().unwrap_or_default())?,
        ),
        _ => Event::Other(kind),
    })
}

/// Header a desktop client puts on its REST calls, so a server holding the
/// web page view-only can tell the desktop's changes from the page's.
pub const CLIENT_HEADER: &str = "x-usdr-client";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_shapes_read_back_and_an_unknown_one_reads_as_normal() {
        for f in FilterShape::ALL {
            let v = serde_json::to_value(f).unwrap();
            assert_eq!(v, f.wire());
            assert_eq!(serde_json::from_value::<FilterShape>(v).unwrap(), f);
        }
        assert_eq!(serde_json::from_str::<FilterShape>("\"gaussian\"").unwrap(), FilterShape::Normal);
        // An older server says nothing about it.
        let st: Status = serde_json::from_str(r#"{"filter_low_hz":300,"filter_high_hz":2800}"#).unwrap();
        assert_eq!(st.filter_shape, FilterShape::Normal);
        let st: Status = serde_json::from_str(r#"{"filter_shape":"soft"}"#).unwrap();
        assert_eq!(st.filter_shape, FilterShape::Soft);
    }

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
    fn stereo_audio_round_trips() {
        let st = StereoReading { channels: 2, pilot: true, blend: 0.6, snr_db: Some(41.4), pilot_dev_hz: Some(7_480.0) };
        let pcm = vec![i16::MIN, i16::MAX, -1, 1, 12_345, -12_345];
        let b = encode_stereo_audio(0, 51_200, &st, &pcm);
        let Binary::Audio(a) = decode_binary(&b).unwrap() else { panic!() };
        assert_eq!((a.rx, a.rate_hz, a.channels()), (0, 51_200, 2));
        assert_eq!(a.samples, pcm, "16-bit samples come back bit for bit");
        let got = a.stereo.unwrap();
        assert!(got.pilot && (got.blend - 0.6).abs() < 0.5 / 255.0 + 1e-6);
        assert_eq!((got.snr_db, got.pilot_dev_hz), (Some(41.0), Some(7_480.0)));
        // Mono, no pilot: nothing claimed that was not measured.
        let mono = StereoReading { channels: 1, ..Default::default() };
        let Binary::Audio(a) = decode_binary(&encode_stereo_audio(1, 51_200, &mono, &[3, 4, 5])).unwrap() else { panic!() };
        let got = a.stereo.unwrap();
        assert_eq!((a.rx, a.channels(), a.samples.len()), (1, 1, 3));
        assert!(!got.pilot && got.snr_db.is_none() && got.pilot_dev_hz.is_none());
        // An odd number of samples cannot be two channels.
        assert!(decode_binary(&encode_stereo_audio(0, 51_200, &st, &[1, 2, 3])).is_err());
        assert!(decode_binary(&b[..9]).is_err());
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
    fn decodes_rds_in_status() {
        let s = r#"{"type":"status","mode":"wfm","rds":{"pi":"54A8","pi_call":"KKQE","ps":"KXYZ FM","pty":9,"pty_name":"Top 40","tp":true,"ta":false,"rt":"Blue Horizon by The Examples","title":"Blue Horizon","locked":true,"pilot":true,"bler":0.04,"updated_ms":5}}"#;
        let Event::Status(st) = decode_text(s).unwrap() else { panic!() };
        let r = st.rds.unwrap();
        assert_eq!(r.ps.as_deref(), Some("KXYZ FM"));
        assert_eq!(r.pty_name.as_deref(), Some("Top 40"));
        assert_eq!(r.artist, None);
        assert!(r.locked && r.pilot);
    }

    #[test]
    fn fax_lines_round_trip() {
        let px: Vec<u8> = (0..1809).map(|i| (i % 256) as u8).collect();
        let b = encode_fax_line(1, 7, 42, &px);
        let Binary::FaxLine(l) = decode_binary(&b).unwrap() else { panic!() };
        assert_eq!((l.rx, l.image, l.line, l.pixels.len()), (1, 7, 42, 1809));
        assert_eq!(l.pixels, px);
        assert!(decode_binary(&b[..b.len() - 1]).is_err(), "a short row is refused");
    }

    #[test]
    fn decodes_station() {
        let t = r#"{"type":"station","station":{"callsign":"K1ABC","grid":"FN31pr","antenna":"","report_spots":true},"reporter":{"state":"waiting","spots_sent":12,"queued":3}}"#;
        let Event::Station(st, rep) = decode_text(t).unwrap() else { panic!() };
        assert!(st.complete() && st.report_spots);
        assert_eq!((rep.state, rep.spots_sent, rep.queued), (ReporterState::Waiting, 12, 3));
        for m in Mode::ALL {
            let back: Mode = serde_json::from_value(serde_json::json!(m.wire())).unwrap();
            assert_eq!(back, m);
        }
    }

    #[test]
    fn scan_cfg_keeps_fields_it_does_not_know() {
        let j = r#"{"start_hz":1,"stop_hz":2,"modes":["am"],"threshold_db":9,"resume_delay_s":1,"slots":4}"#;
        let c: ScanCfg = serde_json::from_str(j).unwrap();
        let back = serde_json::to_value(&c).unwrap();
        assert_eq!(back["slots"], 4);
    }
}
