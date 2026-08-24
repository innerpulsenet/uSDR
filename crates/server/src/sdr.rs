//! Live spectrum and waterfall SDR mode with real-time Digital Signal Classifier.
//!
//! Streams FFT power estimates of up to 2.048 MHz bandwidth from a selected
//! RTL-SDR dongle and classifies digital/analog traffic on inspected signals.

use anyhow::{Context, Result, bail};
use scannerd_dsp::{DecodeChain, FrontEnd, Spectrum, smooth_bins};
use scannerd_engine::aprs::AprsDecoder;
use scannerd_engine::dmr::{DMR_BANDWIDTH_HZ, DmrChannelReceiver, DmrSpec};
use scannerd_engine::flex::FlexDecoder;
use scannerd_engine::p25::C4FM_BANDWIDTH_HZ;
use scannerd_engine::p25::conventional::{P25ChannelReceiver, P25Spec};
use scannerd_engine::pocsag::PocsagDecoder;
use scannerd_engine::{CallEvent, ToneCode};
use scannerd_engine::nbfm::NbfmDemod;
use scannerd_engine::{ClassificationResult, DecodeEvent, SignalClassifier};
use scannerd_radio::{Cmd, Device, DeviceConfig, Role, device};
use crate::devices as config;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

pub const DEFAULT_FREQ_HZ: f64 = 155_000_000.0;
pub const DEFAULT_RATE_HZ: f64 = 2_048_000.0;
pub const DEFAULT_FFT_SIZE: usize = 1024;
pub const DEFAULT_FPS: u32 = 25;
pub const INSPECT_RATE: f64 = 48_000.0;
/// Widest channel the inspector has to pass. The classifier narrows this per
/// protocol candidate; this is only the outer bound.
pub const INSPECT_BANDWIDTH_HZ: f32 = 25_000.0;
const CAPTURE_SECONDS: usize = 20;
/// How often to retry opening the dongle after the radio thread has died.
/// Long enough that a dongle yanked out of its port does not spin the CPU,
/// short enough that a recoverable I2C fault costs a hiccup, not a session.
const RECOVER_INTERVAL: Duration = Duration::from_secs(2);
/// When to conclude the tuner is wedged rather than merely glitching.
///
/// Reopening costs the operator their stream: the samples stop, the waterfall
/// clears and the audio breaks. That is a heavy price, and on a tired dongle
/// it buys about one successful retune. So this is deliberately reluctant —
/// many failures, over a long window, *and* nothing having tuned successfully
/// in all that time. A radio that is merely dropping the occasional write
/// keeps running and simply reports the tunes it would not take.
const CTL_FAULT_WINDOW: Duration = Duration::from_secs(30);
const CTL_FAULT_LIMIT: usize = 6;
/// No successful tune for this long is the other half of the test.
const CTL_QUIET_BEFORE_REOPEN: Duration = Duration::from_secs(30);
/// How long a "cannot keep up" notice stays up after the last dropped block.
const LAG_WARN_HOLD: Duration = Duration::from_secs(4);
/// Longest gap between reopen attempts once reopening has stopped helping.
const RECOVER_INTERVAL_MAX: Duration = Duration::from_secs(30);
/// Minimum spacing between control writes to the tuner.
///
/// Every tune or gain change is a burst of I2C traffic that the radio thread
/// must stop reading the USB bulk stream to perform, and starving that stream
/// is what stalls the control endpoint (`i2c wr failed=-9`). Measured on an
/// R820T: retuning every 30 ms fails about one time in forty, every 250 ms
/// does not fail at all. Dragging a slider or clicking around the waterfall
/// therefore collapses to one write per interval, carrying the latest value.
const CTL_MIN_INTERVAL: Duration = Duration::from_millis(220);

/// How many audio-scope samples ride on each frame. The rate they are
/// decimated to is per-mode; see `SdrMode::scope_rate_hz`.
const SCOPE_SAMPLES: usize = 512;

/// Wideband FM: 200 kHz of channel, discriminated at a rate high enough to
/// carry ±75 kHz of deviation, then dropped to a normal audio rate.
pub const WFM_BANDWIDTH_HZ: f32 = 200_000.0;
pub const WFM_IF_RATE: f64 = 256_000.0;
pub const WFM_DEVIATION_HZ: f32 = 75_000.0;
/// Broadcast FM in the Americas and Korea is 75 µs de-emphasis; 50 µs
/// elsewhere. Fixed at 75 µs here rather than pretending to detect it.
pub const WFM_DEEMPHASIS_TAU: f32 = 75e-6;
/// Packet listens through a normal narrow channel — 1200 baud AFSK lives
/// inside the same 12.5/25 kHz slot voice does.
pub const PACKET_BANDWIDTH_HZ: f32 = 15_000.0;

/// How far the hardware LO is placed above the frequency being displayed when
/// the LO offset is on, as a fraction of the sampled rate.
///
/// An RTL front end puts three things in fixed places: a DC offset and LO
/// leakage at whatever the tuner is tuned to, and the RTL2832's own spur at
/// ±fs/4. Offsetting by fs/4 — the obvious choice — parks the display centre
/// straight on that spur, which measured 11 dB above the noise floor here
/// against 2 dB for the DC residue it was meant to avoid. So the window is
/// placed midway between DC and the fs/4 spur instead, with a width that keeps
/// both outside it.
///
/// This costs most of the span, which is why it is off by default: the DC
/// blocker in `FrontEnd` already flattens the centre to about 2 dB, and that
/// is the better trade for a panadapter. Turn it on when a weak signal is
/// sitting close enough to the centre that even that residue is in the way.
pub const LO_OFFSET_FRACTION: f64 = 0.125;
/// Fraction of the sampled rate that stays clean either side of that offset.
pub const LO_USABLE_FRACTION: f64 = 0.20;

/// How far in the display can zoom, and how many bins it aims to keep across
/// the window. Zooming raises the FFT size with the zoom factor, so a narrower
/// window is genuinely finer-grained rather than the same data stretched wider.
pub const ZOOM_MAX: f64 = 32.0;
const ZOOM_TARGET_BINS: usize = 1024;

/// What the inspect chain is being used for. This picks the channel width, the
/// demodulator, and what gets fed the resulting audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SdrMode {
    /// Narrowband FM plus the full digital classifier. The default.
    #[default]
    Nfm,
    /// Broadcast FM, for listening. The classifier is not run: none of what it
    /// looks for exists in a 200 kHz music channel.
    Wfm,
    /// Narrowband FM run through every packet and paging decoder at once —
    /// AX.25 at 1200 baud, POCSAG at 512/1200/2400, and FLEX — so whatever is
    /// in the passband is decoded without having to be told which it is.
    Packet,
    /// P25 Phase 1 C4FM, decoded to voice through the IMBE vocoder.
    P25,
    /// DMR Tier II, decoded to voice through the AMBE vocoder.
    Dmr,
    /// Everything at once: the classifier, both digital voice receivers, and
    /// every packet and paging decoder, all on the same channel. Costs more
    /// CPU than picking one, and answers "what is this?" without being told.
    Auto,
}

impl SdrMode {
    fn bandwidth_hz(self) -> f32 {
        match self {
            SdrMode::Nfm => INSPECT_BANDWIDTH_HZ,
            SdrMode::Wfm => WFM_BANDWIDTH_HZ,
            SdrMode::Packet => PACKET_BANDWIDTH_HZ,
            SdrMode::P25 => C4FM_BANDWIDTH_HZ,
            SdrMode::Dmr => DMR_BANDWIDTH_HZ,
            // Wide enough for every narrowband candidate at once.
            SdrMode::Auto => INSPECT_BANDWIDTH_HZ,
        }
    }


    /// What the scope trace represents, so the browser knows which instrument
    /// to draw. A 0–4 kHz audio spectrum says nothing useful about C4FM, and an
    /// eye diagram says nothing useful about a broadcast station.
    fn scope_kind(self) -> &'static str {
        match self {
            // Auto is watching for digital, so the eye is the useful view.
            SdrMode::P25 | SdrMode::Dmr | SdrMode::Auto => "symbols",
            SdrMode::Wfm => "mpx",
            _ => "audio",
        }
    }

    /// Symbol rate for the modes whose scope is drawn against one.
    fn symbol_rate_hz(self) -> f32 {
        match self {
            SdrMode::P25 | SdrMode::Dmr | SdrMode::Auto => 4_800.0,
            SdrMode::Packet => 1_200.0,
            _ => 0.0,
        }
    }

    /// Rate the audio scope trace is sent at. Its FFT spans half of this, so
    /// the scope's frequency axis covers the audio the mode actually produces
    /// — 4 kHz of a voice channel, 16 kHz of a broadcast one.
    fn scope_rate_hz(self) -> f32 {
        match self {
            // The whole FM multiplex: 19 kHz pilot, the 23–53 kHz stereo
            // subcarrier and RDS at 57 kHz all need to be inside Nyquist.
            SdrMode::Wfm => 128_000.0,
            // Symbols are sent at the channel rate — decimating an eye
            // diagram destroys the thing it is meant to show.
            SdrMode::P25 | SdrMode::Dmr | SdrMode::Auto => 48_000.0,
            _ => 8_000.0,
        }
    }

    fn target_rate(self) -> f64 {
        match self {
            SdrMode::Wfm => WFM_IF_RATE,
            _ => INSPECT_RATE,
        }
    }

}

/// One block of demodulated audio on its way to the browser.
#[derive(Clone, Debug)]
pub struct AudioFrame {
    pub rate_hz: u32,
    pub samples: Vec<i16>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SdrCfg {
    pub serial: Option<String>,
    pub freq_hz: f64,
    pub rate_hz: f64,
    pub gain_db: Option<f64>,
    pub ppm: f64,
    pub fft_size: usize,
    #[serde(default)]
    pub mode: SdrMode,
    #[serde(default)]
    pub lo_offset: bool,
    #[serde(default)]
    pub clip_guard: bool,
}

impl Default for SdrCfg {
    fn default() -> Self {
        Self {
            serial: None,
            freq_hz: DEFAULT_FREQ_HZ,
            rate_hz: DEFAULT_RATE_HZ,
            gain_db: None,
            ppm: 0.0,
            fft_size: DEFAULT_FFT_SIZE,
            mode: SdrMode::default(),
            lo_offset: false,
            clip_guard: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct SdrStatus {
    pub serial: String,
    pub tuner: String,
    pub freq_hz: f64,
    pub rate_hz: f64,
    pub gain_db: Option<f64>,
    pub ppm: f64,
    pub min_freq_hz: f64,
    pub max_freq_hz: f64,
    pub fft_size: usize,
    pub peak_iq: f32,
    pub inspect_hz: f64,
    /// Rate the inspect chain really delivers, and the rate the classifier is
    /// clocked from. Only equals `INSPECT_RATE` when the span divides evenly.
    #[serde(default)]
    pub inspect_rate_hz: f64,
    /// Blocks the driver produced that no consumer had room for, and
    /// deliveries this loop was too far behind to take.
    #[serde(default)]
    pub dropped_blocks: u64,
    #[serde(default)]
    pub lagged_blocks: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub mode: SdrMode,
    /// Rate the audio stream is delivered at, so the browser can play it
    /// without guessing.
    #[serde(default)]
    pub audio_rate_hz: f64,
    /// Whether the tuner is parked off-centre to keep its DC spur out of the
    /// displayed span, and the span that leaves usable.
    #[serde(default)]
    pub lo_offset: bool,
    #[serde(default)]
    pub lo_offset_hz: f64,
    /// Whether the automatic clip guard is allowed to move the gain.
    #[serde(default)]
    pub clip_guard: bool,
    /// Display magnification. 1.0 shows the whole usable span.
    #[serde(default)]
    pub zoom: f64,
    /// Width of the channel the inspector is listening through, so the display
    /// can draw the passband it is actually using.
    #[serde(default)]
    pub bandwidth_hz: f64,
    /// Receiver-generated spurs being notched, as offsets from the display
    /// centre in Hz.
    #[serde(default)]
    pub spurs_hz: Vec<f64>,
    /// How far the tuned channel's energy actually sits from where the
    /// receiver put it, smoothed. This is what a ppm calibration is measured
    /// from, and what tells the operator the dial is lying.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freq_error_hz: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeakMarker {
    pub freq_hz: f64,
    pub pwr_db: f32,
    pub snr_db: f32,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SdrEvent {
    #[serde(rename_all = "camelCase")]
    Fft {
        center_hz: f64,
        rate_hz: f64,
        min_db: f32,
        max_db: f32,
        /// Power in dB for each FFT bin (size = fft_size).
        pwr: Vec<f32>,
        peak_iq: f32,
        inspect_hz: f64,
        classification: Option<ClassificationResult>,
        peaks: Vec<PeakMarker>,
        /// Demodulated audio for the scope, decimated to `scope_rate_hz` and
        /// scaled to i16 — a fraction of the JSON a float trace would cost.
        scope: Vec<i16>,
        scope_rate_hz: f32,
        /// "audio", "mpx" or "symbols" — which instrument to draw.
        scope_kind: &'static str,
        symbol_rate_hz: f32,
        /// Power in the tuned channel and the noise floor beneath it, measured
        /// from the spectrum so every mode reports the same thing.
        channel_dbfs: f32,
        noise_dbfs: f32,
    },
    #[serde(rename_all = "camelCase")]
    Status(SdrStatus),
    #[serde(rename_all = "camelCase")]
    Decode { inspect_hz: f64, event: DecodeEvent },
}

#[derive(Default)]
struct CaptureBuffer {
    inspect_hz: f64,
    /// Rate these samples were really taken at. The chain lands on
    /// `fs_in / round(fs_in / INSPECT_RATE)`, which is only INSPECT_RATE when
    /// the span divides evenly, so a capture labelled 48 kHz would be a lie at
    /// five of the six spans the UI offers.
    fs_hz: f64,
    voice: VecDeque<i16>,
    discriminator: VecDeque<i16>,
    /// Inspected complex baseband at `fs_hz`, interleaved as I/Q when exported.
    iq: VecDeque<(i16, i16)>,
}

impl CaptureBuffer {
    fn clear(&mut self, inspect_hz: f64, fs_hz: f64) {
        self.inspect_hz = inspect_hz;
        self.fs_hz = fs_hz;
        self.voice.clear();
        self.discriminator.clear();
        self.iq.clear();
    }

    fn rate(&self) -> u32 {
        if self.fs_hz > 0.0 {
            self.fs_hz.round() as u32
        } else {
            INSPECT_RATE as u32
        }
    }

    fn append(
        &mut self,
        inspect_hz: f64,
        fs_hz: f64,
        iq: &[num_complex::Complex32],
        voice: &[f32],
        discriminator_hz: &[f32],
    ) {
        if (self.inspect_hz - inspect_hz).abs() >= 1.0 || (self.fs_hz - fs_hz).abs() >= 1.0 {
            self.clear(inspect_hz, fs_hz);
        }
        let max = fs_hz.max(1.0) as usize * CAPTURE_SECONDS;
        self.voice.extend(
            voice
                .iter()
                .map(|&x| (x.clamp(-1.0, 1.0) * 28_000.0) as i16),
        );
        self.discriminator.extend(
            discriminator_hz
                .iter()
                .map(|&x| ((x / 6_000.0).clamp(-1.0, 1.0) * 28_000.0) as i16),
        );
        self.iq.extend(iq.iter().map(|x| {
            (
                (x.re.clamp(-1.0, 1.0) * 32_000.0) as i16,
                (x.im.clamp(-1.0, 1.0) * 32_000.0) as i16,
            )
        }));
        while self.voice.len() > max {
            self.voice.pop_front();
        }
        while self.discriminator.len() > max {
            self.discriminator.pop_front();
        }
        while self.iq.len() > max {
            self.iq.pop_front();
        }
    }
}

pub struct SdrRuntime {
    pub status: Arc<Mutex<SdrStatus>>,
    pub calls: Arc<Mutex<VecDeque<RecordedCall>>>,
    stop: Arc<AtomicBool>,
    cmd_tx: std::sync::mpsc::Sender<SdrCmd>,
    worker: Option<std::thread::JoinHandle<()>>,
    capture: Arc<Mutex<CaptureBuffer>>,
}

enum SdrCmd {
    Tune(f64),
    Inspect(f64),
    Gain(Option<f64>),
    Rate(f64),
    SwitchDevice(String),
    Mode(SdrMode),
    LoOffset(bool),
    ClipGuard(bool),
    /// Offsets from the display centre, in Hz, that a spur check identified as
    /// generated inside the receiver.
    Spurs(Vec<f64>),
    Ppm(f64),
    Zoom(f64),
}

impl Drop for SdrRuntime {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

impl SdrRuntime {
    pub fn tune(&self, freq_hz: f64) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::Tune(freq_hz))
            .map_err(|e| anyhow::anyhow!("send tune: {e}"))
    }

    pub fn inspect(&self, freq_hz: f64) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::Inspect(freq_hz))
            .map_err(|e| anyhow::anyhow!("send inspect: {e}"))
    }

    pub fn set_gain(&self, gain_db: Option<f64>) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::Gain(gain_db))
            .map_err(|e| anyhow::anyhow!("send gain: {e}"))
    }

    pub fn set_rate(&self, rate_hz: f64) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::Rate(rate_hz))
            .map_err(|e| anyhow::anyhow!("send rate: {e}"))
    }

    pub fn set_mode(&self, mode: SdrMode) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::Mode(mode))
            .map_err(|e| anyhow::anyhow!("send mode: {e}"))
    }

    pub fn set_lo_offset(&self, on: bool) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::LoOffset(on))
            .map_err(|e| anyhow::anyhow!("send lo offset: {e}"))
    }

    pub fn set_zoom(&self, zoom: f64) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::Zoom(zoom))
            .map_err(|e| anyhow::anyhow!("send zoom: {e}"))
    }

    /// Crystal correction, in parts per million.
    pub fn set_ppm(&self, ppm: f64) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::Ppm(ppm))
            .map_err(|e| anyhow::anyhow!("send ppm: {e}"))
    }

    pub fn set_spurs(&self, offsets_hz: Vec<f64>) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::Spurs(offsets_hz))
            .map_err(|e| anyhow::anyhow!("send spurs: {e}"))
    }

    pub fn set_clip_guard(&self, on: bool) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::ClipGuard(on))
            .map_err(|e| anyhow::anyhow!("send clip guard: {e}"))
    }

    pub fn switch_device(&self, serial: String) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::SwitchDevice(serial))
            .map_err(|e| anyhow::anyhow!("send switch: {e}"))
    }

    pub fn capture_wav(&self, kind: CaptureKind) -> (f64, usize, Vec<u8>) {
        let capture = self.capture.lock().expect("SDR capture");
        let rate = capture.rate();
        match kind {
            CaptureKind::Voice => {
                let samples: Vec<i16> = capture.voice.iter().copied().collect();
                let count = samples.len();
                (capture.inspect_hz, count, pcm_wav(&samples, rate, 1))
            }
            CaptureKind::Discriminator => {
                let samples: Vec<i16> = capture.discriminator.iter().copied().collect();
                let count = samples.len();
                (capture.inspect_hz, count, pcm_wav(&samples, rate, 1))
            }
            CaptureKind::Iq => {
                let mut samples = Vec::with_capacity(capture.iq.len() * 2);
                for &(i, q) in &capture.iq {
                    samples.extend([i, q]);
                }
                let count = capture.iq.len();
                (capture.inspect_hz, count, pcm_wav(&samples, rate, 2))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureKind {
    Voice,
    Discriminator,
    Iq,
}

/// WAV wrapper for a recorded call.
pub fn pcm_wav_public(samples: &[i16], sample_rate: u32, channels: u16) -> Vec<u8> {
    pcm_wav(samples, sample_rate.max(8_000), channels)
}

fn pcm_wav(samples: &[i16], sample_rate: u32, channels: u16) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * u32::from(channels) * 2).to_le_bytes());
    out.extend_from_slice(&(channels * 2).to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayResult {
    pub frequency_hz: f64,
    pub sample_rate_hz: u32,
    pub samples: usize,
    pub duration_s: f32,
    pub classification: ClassificationResult,
    pub events: Vec<DecodeEvent>,
}

/// Deterministically replay a stereo I/Q WAV produced by `capture.wav?kind=iq`.
pub fn replay_iq_wav(wav: &[u8], frequency_hz: f64) -> Result<ReplayResult> {
    if wav.len() < 44 || &wav[..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        bail!("not a PCM WAV capture");
    }
    let channels = u16::from_le_bytes([wav[22], wav[23]]);
    let sample_rate_hz = u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]);
    let bits = u16::from_le_bytes([wav[34], wav[35]]);
    if channels != 2 || bits != 16 {
        bail!("replay requires a 16-bit stereo I/Q WAV");
    }
    // The classifier is built from the rate in the file rather than a fixed
    // one: captures are taken at whatever the inspect chain lands on for the
    // span in use, and every decoder below counts samples per symbol.
    if !(8_000..=192_000).contains(&sample_rate_hz) {
        bail!("replay sample rate {sample_rate_hz} Hz is outside the usable range");
    }
    let mut iq = Vec::with_capacity((wav.len() - 44) / 4);
    for frame in wav[44..].chunks_exact(4) {
        let i = i16::from_le_bytes([frame[0], frame[1]]) as f32 / 32_000.0;
        let q = i16::from_le_bytes([frame[2], frame[3]]) as f32 / 32_000.0;
        iq.push(num_complex::Complex32::new(i, q));
    }
    let mut classifier = SignalClassifier::new(f64::from(sample_rate_hz));
    let mut classification = ClassificationResult::default();
    let mut events = Vec::new();
    for block in iq.chunks(4096) {
        classification = classifier.process(block, frequency_hz);
        events.extend(classifier.take_decode_events());
    }
    Ok(ReplayResult {
        frequency_hz,
        sample_rate_hz,
        samples: iq.len(),
        duration_s: iq.len() as f32 / sample_rate_hz as f32,
        classification,
        events,
    })
}

/// Whether the LO offset can actually be used for this mode and span.
///
/// The offset costs most of the span, and a 200 kHz broadcast channel does not
/// fit in what a 2.048 MHz span has left — the passband would fill half the
/// display and there would be nothing either side of it to look at. Below a
/// third of the window the channel still leaves usable context; above that the
/// offset is declined and the full span is shown instead.
fn lo_active(rate_hz: f64, on: bool, mode: SdrMode) -> bool {
    on && f64::from(mode.bandwidth_hz()) <= rate_hz * LO_USABLE_FRACTION / 3.0
}

/// LO offset for a span, or zero when the offset is off or does not fit.
fn lo_offset_for(rate_hz: f64, on: bool, mode: SdrMode) -> f64 {
    if lo_active(rate_hz, on, mode) {
        rate_hz * LO_OFFSET_FRACTION
    } else {
        0.0
    }
}

/// The span actually shown, which shrinks when the LO is parked off-centre.
fn display_rate_for(rate_hz: f64, lo_on: bool, mode: SdrMode) -> f64 {
    if lo_active(rate_hz, lo_on, mode) {
        rate_hz * LO_USABLE_FRACTION
    } else {
        rate_hz
    }
}

fn open_sdr(
    serial: Option<&str>,
    freq_hz: f64,
    rate_hz: f64,
    gain_db: Option<f64>,
    lo_offset_hz: f64,
) -> Result<(Device, String, String)> {
    let all = device::enumerate()?;
    if all.is_empty() {
        bail!("no RTL-SDR device found");
    }
    let info = if let Some(s) = serial {
        all.iter()
            .find(|d| d.serial == s)
            .with_context(|| format!("SDR serial {s} not found on USB"))?
    } else {
        all.first().unwrap()
    };
    let mut cfg = DeviceConfig::new(&info.serial, freq_hz + lo_offset_hz, rate_hz);
    cfg.gain = gain_db;
    cfg.ppm = config::load().unwrap_or_default().ppm_for(&info.serial);
    // With the LO parked off-centre the analog IF has to reach past the
    // offset, or the tuner filters away the half of the span it was moved
    // toward. Zero asks the driver for its widest.
    cfg.cover_hz = if lo_offset_hz != 0.0 {
        scannerd_radio::cover_hz_offset(lo_offset_hz, rate_hz * LO_USABLE_FRACTION)
    } else {
        0.0
    };
    cfg.role = Role::Sdr;
    let dev = device::open(&cfg)?;
    Ok((dev, info.serial.clone(), info.tuner.clone()))
}

pub fn spawn(
    cfg: SdrCfg,
    events: broadcast::Sender<SdrEvent>,
    audio: broadcast::Sender<AudioFrame>,
) -> Result<SdrRuntime> {
    let status = Arc::new(Mutex::new(SdrStatus::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    let capture = Arc::new(Mutex::new(CaptureBuffer::default()));
    let calls: Arc<Mutex<VecDeque<RecordedCall>>> = Arc::new(Mutex::new(VecDeque::new()));

    let thread_status = Arc::clone(&status);
    let thread_stop = Arc::clone(&stop);
    let thread_events = events;
    let thread_audio = audio;
    let thread_capture = Arc::clone(&capture);
    let thread_calls = Arc::clone(&calls);

    let worker = std::thread::spawn(move || {
        if let Err(e) = run_sdr(
            cfg,
            thread_events,
            thread_audio,
            thread_status,
            thread_stop,
            cmd_rx,
            thread_capture,
            thread_calls,
        ) {
            eprintln!("SDR worker stopped with error: {e:#}");
        }
    });

    Ok(SdrRuntime {
        status,
        stop,
        cmd_tx,
        worker: Some(worker),
        capture,
        calls,
    })
}

/// Build the inspect chain for a span, together with a classifier told the
/// rate that chain will really produce.
///
/// `DecodeChain` lands on `fs_in / round(fs_in / INSPECT_RATE)`. Only 1.536
/// MHz of the spans the UI offers divides 48 kHz evenly; 2.048 MHz gives
/// 47 627.91 Hz and 256 kHz gives 51 200 Hz. Handing the classifier the
/// nominal 48 kHz was a 0.8-6.7% clock error on every symbol-timing decoder
/// it owns, which does not merely lose frames — the slip walks a POCSAG batch
/// out of alignment and the BCH corrector accepts the debris as new address
/// words, so it invents pages that were never transmitted.
fn build_inspect(rate_hz: f64, offset_hz: f64, mode: SdrMode) -> (DecodeChain, SignalClassifier) {
    let mut chain = DecodeChain::new(rate_hz, mode.bandwidth_hz(), mode.target_rate());
    chain.set_offset(offset_hz);
    let classifier = SignalClassifier::new(chain.fs_out());
    (chain, classifier)
}

/// Everything a mode needs on top of the shared inspect chain.
///
/// NFM leans on the classifier, which already demodulates for its own use, so
/// there is nothing extra to build. WFM and PACKET each need their own
/// demodulator, and both want the audio the classifier would not produce.
enum Demod {
    Nfm,
    Wfm {
        fm: NbfmDemod,
        disc: Vec<f32>,
        deemph: scannerd_dsp::OnePole,
        /// Integer decimation from the 256 kHz IF down to something a sound
        /// card is happy with, with the accumulator carried between blocks.
        decim: usize,
        phase: usize,
        acc: f32,
    },
    Packet {
        aprs: AprsDecoder,
        /// One decoder per POCSAG rate: a pager channel does not announce
        /// which it uses, and running all three costs less than guessing.
        pocsag: Vec<PocsagDecoder>,
        flex: FlexDecoder,
    },
    P25 {
        rx: Box<P25ChannelReceiver>,
    },
    Dmr {
        rx: Box<DmrChannelReceiver>,
    },
    /// Every decoder at once. The classifier still runs, because it is what
    /// identifies the analogue and one-off protocols the dedicated receivers
    /// know nothing about.
    Auto {
        p25: Box<P25ChannelReceiver>,
        dmr: Box<DmrChannelReceiver>,
        aprs: AprsDecoder,
        pocsag: Vec<PocsagDecoder>,
        flex: FlexDecoder,
    },
}

impl Demod {
    /// `fs_chain` is the inspect chain's output rate; the digital voice
    /// receivers instead take the whole span, because they do their own
    /// channel extraction and want the widest view of it.
    fn new(
        mode: SdrMode,
        fs_chain: f64,
        span_rate_hz: f64,
        span_center_hz: f64,
        channel_hz: f64,
    ) -> Self {
        let fs_out = fs_chain;
        match mode {
            SdrMode::P25 => Demod::P25 {
                rx: Box::new(P25ChannelReceiver::new(
                    P25Spec {
                        name: "inspect".into(),
                        freq_hz: channel_hz,
                        // Accept whatever NAC is on the air: the operator
                        // pointed the cursor at it, that is the filter.
                        nac: None,
                    },
                    span_rate_hz,
                    span_center_hz,
                )),
            },
            SdrMode::Dmr => Demod::Dmr {
                rx: Box::new(DmrChannelReceiver::new(
                    DmrSpec {
                        name: "inspect".into(),
                        freq_hz: channel_hz,
                        color_code: None,
                        slot: None,
                    },
                    span_rate_hz,
                    span_center_hz,
                )),
            },
            SdrMode::Auto => Demod::Auto {
                p25: Box::new(P25ChannelReceiver::new(
                    P25Spec { name: "auto".into(), freq_hz: channel_hz, nac: None },
                    span_rate_hz,
                    span_center_hz,
                )),
                dmr: Box::new(DmrChannelReceiver::new(
                    DmrSpec {
                        name: "auto".into(),
                        freq_hz: channel_hz,
                        color_code: None,
                        slot: None,
                    },
                    span_rate_hz,
                    span_center_hz,
                )),
                aprs: AprsDecoder::new(fs_out),
                pocsag: [512u32, 1200, 2400]
                    .into_iter()
                    .map(|baud| PocsagDecoder::new(fs_out, baud))
                    .collect(),
                flex: FlexDecoder::new(fs_out),
            },
            SdrMode::Nfm => Demod::Nfm,
            SdrMode::Wfm => {
                let decim = (fs_out / 48_000.0).round().max(1.0) as usize;
                Demod::Wfm {
                    fm: NbfmDemod::with_deviation(fs_out, WFM_DEVIATION_HZ),
                    disc: Vec::new(),
                    // One-pole de-emphasis, its time constant expressed in
                    // samples at the rate the discriminator runs.
                    deemph: scannerd_dsp::OnePole::new(WFM_DEEMPHASIS_TAU * fs_out as f32),
                    decim,
                    phase: 0,
                    acc: 0.0,
                }
            }
            SdrMode::Packet => Demod::Packet {
                aprs: AprsDecoder::new(fs_out),
                pocsag: [512u32, 1200, 2400]
                    .into_iter()
                    .map(|baud| PocsagDecoder::new(fs_out, baud))
                    .collect(),
                flex: FlexDecoder::new(fs_out),
            },
        }
    }

    /// Audio rate this demodulator delivers, given the chain rate feeding it.
    fn audio_rate(&self, fs_out: f64) -> f64 {
        match self {
            Demod::Wfm { decim, .. } => fs_out / *decim as f64,
            Demod::P25 { rx } => rx.audio_rate(),
            Demod::Dmr { rx } => rx.audio_rate(),
            // Whichever receiver is carrying a call sets the rate; otherwise
            // the analogue path is what is being heard.
            Demod::Auto { p25, dmr, .. } => {
                if p25.in_call() {
                    p25.audio_rate()
                } else if dmr.in_call() {
                    dmr.audio_rate()
                } else {
                    fs_out
                }
            }
            _ => fs_out,
        }
    }
}

/// A digital voice call that has finished, kept so it can be listened to
/// again.
///
/// This exists because a decoded call and an audible one are not the same
/// thing: the receivers vocode encrypted voice to silence rather than to
/// noise, so a call can be reported in full and still be silent. Keeping the
/// audio alongside the reason lets the operator tell "I missed it" from
/// "there was nothing to hear".
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedCall {
    pub id: u64,
    pub protocol: String,
    /// Wall-clock start, milliseconds since the epoch.
    pub started_ms: u64,
    pub duration_s: f32,
    pub freq_hz: f64,
    pub rate_hz: u32,
    pub samples: usize,
    /// Peak absolute sample, so a silent recording is obvious without playing it.
    pub peak: f32,
    pub encrypted: bool,
    pub decrypted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub algorithm: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tone: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_id: Option<u32>,
    #[serde(skip)]
    pub audio: Vec<i16>,
}

/// How many finished calls to keep. A handful is enough to look back over what
/// just happened without letting a busy channel grow without bound.
const RECENT_CALLS: usize = 16;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Codewords as hex, one per group, so a frame can be inspected as it came off
/// the air. Kept as words because POCSAG and FLEX codewords are 20 and 21 bits
/// — packing them into bytes would misalign every one after the first.
fn hex_words(words: &[u32]) -> String {
    words
        .iter()
        .map(|w| format!("{w:06X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// One decoded AX.25 frame, in the shape the decode log already renders.
fn packet_event(packet: &scannerd_engine::aprs::AprsPacket) -> DecodeEvent {
    let mut fields = std::collections::BTreeMap::new();
    fields.insert("from".into(), packet.from.clone());
    fields.insert("to".into(), packet.to.clone());
    if let (Some(lat), Some(lon)) = (packet.lat, packet.lon) {
        fields.insert("lat".into(), format!("{lat:.5}"));
        fields.insert("lon".into(), format!("{lon:.5}"));
    }
    DecodeEvent {
        protocol: "APRS".into(),
        kind: "AX.25 UI frame".into(),
        summary: format!("{} > {} · {}", packet.from, packet.to, packet.text),
        // Only frames whose FCS checked out are handed back by the decoder.
        valid: true,
        fields,
    }
}

fn pocsag_event(msg: &scannerd_engine::PocsagMessage) -> DecodeEvent {
    let mut fields = std::collections::BTreeMap::new();
    fields.insert("capcode".into(), msg.capcode.to_string());
    fields.insert("function".into(), msg.function.to_string());
    fields.insert("baud".into(), msg.baud.to_string());
    fields.insert("correctedBits".into(), msg.corrected_bits.to_string());
    fields.insert("partial".into(), msg.partial.to_string());
    // The native codewords, so the frame can be read rather than just its
    // decoded text. POCSAG's are 20 bits, which is why they are shown as words
    // and not packed into a byte dump that would misalign them.
    if !msg.raw_words.is_empty() {
        fields.insert("rawWords".into(), hex_words(&msg.raw_words));
    }
    DecodeEvent {
        protocol: "POCSAG".into(),
        kind: format!("{} baud page", msg.baud),
        summary: format!("{} · {}", msg.capcode, msg.text),
        valid: true,
        fields,
    }
}

fn flex_event(msg: &scannerd_engine::FlexMessage) -> DecodeEvent {
    let mut fields = std::collections::BTreeMap::new();
    fields.insert("capcode".into(), msg.capcode.to_string());
    fields.insert("cycle".into(), msg.cycle.to_string());
    fields.insert("frame".into(), msg.frame.to_string());
    fields.insert("phase".into(), msg.phase.to_string());
    fields.insert("baud".into(), msg.baud.to_string());
    fields.insert("addressType".into(), msg.address_type.clone());
    fields.insert("fecCorrected".into(), msg.fec_corrected.to_string());
    fields.insert("fecUncorrectable".into(), msg.fec_uncorrectable.to_string());
    if let Some(ok) = msg.payload_checksum_ok {
        fields.insert("payloadChecksumOk".into(), ok.to_string());
    }
    // 21-bit words, again shown as words rather than bytes.
    if !msg.raw_words.is_empty() {
        fields.insert("rawWords".into(), hex_words(&msg.raw_words));
    }
    DecodeEvent {
        protocol: "FLEX".into(),
        kind: "page".into(),
        summary: format!("{} · {}", msg.capcode, msg.text),
        valid: true,
        fields,
    }
}

/// A digital voice call starting or ending, as a decode-log line.
fn call_event(mode: SdrMode, ev: &CallEvent) -> DecodeEvent {
    let protocol = if mode == SdrMode::P25 { "P25" } else { "DMR" };
    match ev {
        CallEvent::Started => DecodeEvent {
            protocol: protocol.into(),
            kind: "call".into(),
            summary: "voice call started".into(),
            valid: true,
            fields: std::collections::BTreeMap::new(),
        },
        CallEvent::Ended(summary) => {
            let mut fields = std::collections::BTreeMap::new();
            fields.insert("seconds".into(), format!("{:.1}", summary.duration_s()));
            fields.insert("peakSnrDb".into(), format!("{:.1}", summary.peak_snr_db));
            if let Some(tone) = &summary.tone {
                let key = match tone {
                    ToneCode::P25 { .. } => "nac",
                    ToneCode::Dmr { .. } => "colorCode",
                    _ => "tone",
                };
                fields.insert(key.into(), tone.label());
            }
            DecodeEvent {
                protocol: protocol.into(),
                kind: "call".into(),
                summary: format!(
                    "voice call ended · {:.1}s{}",
                    summary.duration_s(),
                    summary
                        .tone
                        .as_ref()
                        .map(|t| format!(" · {}", t.label()))
                        .unwrap_or_default()
                ),
                valid: true,
                fields,
            }
        }
    }
}

/// Power in the channel, and the noise floor under it, both in dBFS.
///
/// Taken from the spectrum rather than from the classifier, because the
/// classifier only runs in the narrowband modes — WFM, P25 and DMR bypass it
/// entirely and used to leave the meter reading its default. Summing the bins
/// across the passband gives a level that means the same thing in every mode.
fn channel_level(
    shown: &[f32],
    shown_rate: f64,
    channel_offset_hz: f64,
    bandwidth_hz: f64,
) -> (f32, f32) {
    let n = shown.len();
    if n < 8 {
        return (-120.0, -120.0);
    }
    let bin_of = |hz: f64| ((hz / shown_rate + 0.5) * (n - 1) as f64).round();
    let lo = bin_of(channel_offset_hz - bandwidth_hz / 2.0).clamp(0.0, (n - 1) as f64) as usize;
    let hi = bin_of(channel_offset_hz + bandwidth_hz / 2.0).clamp(0.0, (n - 1) as f64) as usize;
    let (lo, hi) = if hi > lo { (lo, hi) } else { (lo, (lo + 1).min(n - 1)) };

    let mut sum = 0.0f64;
    for &db in &shown[lo..=hi] {
        sum += 10f64.powf(f64::from(db) / 10.0);
    }
    let width = (hi - lo + 1) as f64;

    // Median bin as the floor, scaled to the same width, so the difference is
    // a signal-to-noise ratio rather than a bandwidth ratio.
    let mut sorted = shown.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let floor_lin = 10f64.powf(f64::from(sorted[n / 2]) / 10.0) * width;

    (
        (10.0 * sum.max(1e-30).log10()) as f32,
        (10.0 * floor_lin.max(1e-30).log10()) as f32,
    )
}

/// Power-weighted centre of the channel, relative to where it was expected.
///
/// The discriminator's DC would be the obvious measurement, but on a modulated
/// signal it is dominated by programme content — measured here it wandered
/// between -44 and -61 ppm on adjacent stations that must share one crystal.
/// The spectral centroid averages that away: it asks where the channel's
/// energy actually sits, which is what a frequency calibration needs.
fn carrier_offset_hz(
    shown: &[f32],
    shown_rate: f64,
    channel_offset_hz: f64,
    bandwidth_hz: f64,
) -> Option<f64> {
    let n = shown.len();
    if n < 16 {
        return None;
    }
    let mut sorted = shown.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let floor = sorted[n * 3 / 10];

    let half = bandwidth_hz / 2.0;
    let mut num = 0.0f64;
    let mut den = 0.0f64;
    for (i, &db) in shown.iter().enumerate() {
        let off = (i as f64 / (n - 1) as f64 - 0.5) * shown_rate;
        if (off - channel_offset_hz).abs() > half {
            continue;
        }
        // Power above the noise floor only, so the floor does not drag the
        // centroid toward the middle of the window.
        let lin = 10f64.powf(f64::from(db - floor) / 10.0) - 1.0;
        if lin <= 0.0 {
            continue;
        }
        num += off * lin;
        den += lin;
    }
    (den > 0.0).then(|| num / den - channel_offset_hz)
}

/// Narrow notch for one spur sitting inside the channel being decoded.
///
/// The spur is rotated down to DC, a one-pole high-pass takes it out, and the
/// result is rotated back. Doing this at the inspect chain's output rate costs
/// a few operations per sample instead of the tens of millions per second the
/// same filter would cost across the whole span, and it only ever runs when a
/// spur is actually inside the passband.
struct ChannelNotch {
    nco: scannerd_dsp::Nco,
    back: scannerd_dsp::Nco,
    dc: num_complex::Complex32,
    a: f32,
}

impl ChannelNotch {
    fn new(offset_hz: f64, fs: f64, width_hz: f64) -> Self {
        let mut nco = scannerd_dsp::Nco::new();
        let mut back = scannerd_dsp::Nco::new();
        nco.set_freq(offset_hz, fs);
        back.set_freq(-offset_hz, fs);
        // One-pole corner at half the notch width.
        let tau = (fs / (std::f64::consts::PI * width_hz.max(1.0))) as f32;
        Self {
            nco,
            back,
            dc: num_complex::Complex32::new(0.0, 0.0),
            a: (-1.0 / tau.max(1.0)).exp(),
        }
    }

    fn process(&mut self, iq: &mut Vec<num_complex::Complex32>, scratch: &mut Vec<num_complex::Complex32>) {
        self.nco.mix(iq, scratch);
        for v in scratch.iter_mut() {
            self.dc = self.dc * self.a + *v * (1.0 - self.a);
            *v -= self.dc;
        }
        self.back.mix(scratch, iq);
    }
}

/// Replace the bins a spur occupies with a straight line drawn between the
/// clean bins either side of it.
///
/// This is cosmetic by design: the energy is real, it is just not on the air,
/// and leaving a red spike in the middle of the waterfall trains the eye to
/// see a signal that is not there. Anything wider than the guard is left
/// alone, so a real transmission that happens to sit on the offset still
/// shows — narrowed, but present.
fn notch_display(pwr: &mut [f32], rate_hz: f64, spur_offsets_hz: &[f64], guard_hz: f64) {
    let n = pwr.len();
    if n < 8 || spur_offsets_hz.is_empty() {
        return;
    }
    let bin_of = |hz: f64| -> f64 { (hz / rate_hz + 0.5) * (n - 1) as f64 };
    for &spur in spur_offsets_hz {
        let lo = bin_of(spur - guard_hz).floor() as isize;
        let hi = bin_of(spur + guard_hz).ceil() as isize;
        if hi < 0 || lo >= n as isize {
            continue;
        }
        let lo = lo.max(0) as usize;
        let hi = (hi as usize).min(n - 1);
        if hi <= lo {
            continue;
        }
        // Anchor on the nearest bin outside the notch on each side.
        let left = pwr[lo.saturating_sub(1)];
        let right = pwr[(hi + 1).min(n - 1)];
        let span = (hi - lo) as f32;
        for (k, i) in (lo..=hi).enumerate() {
            let t = if span > 0.0 { k as f32 / span } else { 0.0 };
            pwr[i] = left + (right - left) * t;
        }
    }
}

/// Peak search over the displayed span.
///
/// `spur_offsets_hz` are offsets from `center_hz` where the *hardware* is
/// known to produce something that is not a signal — its own DC/LO leakage and
/// the RTL2832's ±fs/4 images. Surveying the bands here without this returned
/// a list of strong "signals" that all sat at the same baseband offset and
/// followed the tuner as it moved, which is the definition of a spur, and
/// clicking one only ever inspected noise.
fn find_peaks(
    smoothed: &[f32],
    center_hz: f64,
    rate_hz: f64,
    spur_offsets_hz: &[f64],
) -> Vec<PeakMarker> {
    let mut peaks = Vec::new();
    let n = smoothed.len();
    if n < 16 {
        return peaks;
    }
    let mut sorted = smoothed.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let noise_floor = sorted[(n / 5).min(n - 1)];

    let bin_hz = rate_hz / n as f64;

    // Half a channel either side of a spur, so its skirts go with it.
    let spur_guard_hz = 8_000.0_f64.max(bin_hz * 3.0);

    for i in 3..n - 3 {
        let offset_hz = (i as f64 / (n - 1) as f64 - 0.5) * rate_hz;
        if spur_offsets_hz
            .iter()
            .any(|s| (offset_hz - s).abs() <= spur_guard_hz)
        {
            continue;
        }
        let p = smoothed[i];
        if p > noise_floor + 8.0
            && p > smoothed[i - 1]
            && p >= smoothed[i - 2]
            && p > smoothed[i + 1]
            && p >= smoothed[i + 2]
        {
            let freq_hz = center_hz - rate_hz * 0.5 + (i as f64 + 0.5) * bin_hz;
            let snr_db = p - noise_floor;
            peaks.push(PeakMarker {
                freq_hz,
                pwr_db: p,
                snr_db,
            });
        }
    }
    peaks.sort_by(|a, b| b.pwr_db.total_cmp(&a.pwr_db));
    let mut filtered: Vec<PeakMarker> = Vec::new();
    for p in peaks {
        if !filtered
            .iter()
            .any(|q| (q.freq_hz - p.freq_hz).abs() < 18_000.0)
        {
            filtered.push(p);
            if filtered.len() >= 6 {
                break;
            }
        }
    }
    filtered.sort_by(|a, b| a.freq_hz.total_cmp(&b.freq_hz));
    filtered
}

fn run_sdr(
    cfg: SdrCfg,
    events: broadcast::Sender<SdrEvent>,
    audio_tx: broadcast::Sender<AudioFrame>,
    status: Arc<Mutex<SdrStatus>>,
    stop: Arc<AtomicBool>,
    cmd_rx: std::sync::mpsc::Receiver<SdrCmd>,
    capture: Arc<Mutex<CaptureBuffer>>,
    calls: Arc<Mutex<VecDeque<RecordedCall>>>,
) -> Result<()> {
    let mut current_serial = cfg.serial.clone();
    let mut current_freq = cfg.freq_hz;
    let mut current_rate = cfg.rate_hz;
    let mut current_gain = cfg.gain_db;
    let mut current_mode = cfg.mode;
    let mut current_zoom = 1.0f64;
    // What the operator asked for versus what the tuner confirmed. A retune
    // can fail — this dongle stalls its I2C bus often enough that it will —
    // and everything that labels or extracts a frequency has to follow the
    // second one. Believing the request is how the display ends up insisting
    // a station is at 99.1 while the radio is still sitting on 100.
    let mut tuned_freq;
    let mut lo_offset = cfg.lo_offset;
    let mut clip_guard_on = cfg.clip_guard;
    let mut inspect_hz = current_freq;

    // Keep trying rather than dying on the spot. The dongle is routinely busy
    // for a moment after a previous run exits, and a server that gives up
    // there stays dead until someone notices and restarts it — with no radio,
    // the recovery loop below never gets to run at all.
    let (dev, serial, tuner) = loop {
        match open_sdr(
            current_serial.as_deref(),
            current_freq,
            current_rate,
            current_gain,
            lo_offset_for(current_rate, lo_offset, current_mode),
        ) {
            Ok(opened) => break opened,
            Err(e) => {
                if stop.load(Ordering::Relaxed) {
                    return Err(e);
                }
                eprintln!("SDR: waiting for a radio: {e}");
                {
                    let mut s = status.lock().unwrap();
                    s.error = Some(format!("waiting for a radio: {e}"));
                    let _ = events.send(SdrEvent::Status(s.clone()));
                }
                std::thread::sleep(RECOVER_INTERVAL);
            }
        }
    };
    current_serial = Some(serial.clone());

    let mut fft_size = if (256..=8192).contains(&cfg.fft_size) {
        cfg.fft_size
    } else {
        DEFAULT_FFT_SIZE
    };

    let ppm = config::load().unwrap_or_default().ppm_for(&serial);
    {
        let mut s = status.lock().unwrap();
        *s = SdrStatus {
            serial: serial.clone(),
            tuner: tuner.clone(),
            freq_hz: current_freq,
            rate_hz: current_rate,
            gain_db: current_gain,
            ppm,
            min_freq_hz: current_freq - display_rate_for(current_rate, lo_offset, current_mode) / 2.0,
            max_freq_hz: current_freq + display_rate_for(current_rate, lo_offset, current_mode) / 2.0,
            fft_size,
            peak_iq: 0.0,
            inspect_hz,
            inspect_rate_hz: 0.0,
            dropped_blocks: 0,
            lagged_blocks: 0,
            error: None,
            mode: current_mode,
            audio_rate_hz: 0.0,
            lo_offset,
            lo_offset_hz: lo_offset_for(current_rate, lo_offset, current_mode),
            clip_guard: clip_guard_on,
            zoom: current_zoom,
            bandwidth_hz: f64::from(current_mode.bandwidth_hz()),
            spurs_hz: Vec::new(),
            freq_error_hz: None,
        };
    }

    let _ = dev.cmd.send(Cmd::ClipGuard(clip_guard_on));
    tuned_freq = current_freq;
    let rx = dev.iq.subscribe_with_depth(16);
    let mut dev_opt = Some(dev);
    let mut rx_opt = Some(rx);

    let mut spectrum = Spectrum::new(fft_size);
    let mut pwr: Vec<f32> = Vec::with_capacity(fft_size);
    let mut smoothed: Vec<f32> = Vec::with_capacity(fft_size);

    let (mut inspect_chain, mut classifier) =
        build_inspect(
                            current_rate,
                            inspect_hz - current_freq - lo_offset_for(current_rate, lo_offset, current_mode),
                            current_mode,
                        );
    // DC offset and IQ image correction on the raw span, before anything
    // measures or demodulates it. The impulse blanker is deliberately off:
    // it keys on wideband magnitude, so it eats the leading edge of exactly
    // the strong narrowband bursts PACKET is here to decode.
    let mut front = FrontEnd::without_blanker(current_rate);
    let mut clean: Vec<num_complex::Complex32> = Vec::new();
    let mut demod = Demod::new(
                            current_mode,
                            inspect_chain.fs_out(),
                            current_rate,
                            current_freq + lo_offset_for(current_rate, lo_offset, current_mode),
                            inspect_hz,
                        );
    {
        let mut s = status.lock().unwrap();
        s.inspect_rate_hz = inspect_chain.fs_out();
        s.audio_rate_hz = demod.audio_rate(inspect_chain.fs_out());
    }
    let _ = events.send(SdrEvent::Status(status.lock().unwrap().clone()));
    let mut inspect_iq = Vec::new();
    let mut notch_scratch: Vec<num_complex::Complex32> = Vec::new();
    // Instantaneous frequency of the inspected channel, for the eye and
    // constellation displays. The protocol receivers keep their own symbol
    // recovery private, and this only has to be good enough to look at.
    let mut scope_fm = NbfmDemod::with_deviation(inspect_chain.fs_out(), 3_000.0);
    let mut scope_disc: Vec<f32> = Vec::new();
    // Whatever this frame's scope trace is, normalised to ±1.
    let mut scope_src: Vec<f32> = Vec::new();
    let mut scope_src_rate;
    // Full-scale deviation of whatever `scope_src` holds, so a normalised
    // trace can be turned back into Hz for the signal readout.
    let mut scope_dev_scale = 0.0f32;
    // Audio of the call in progress, plus when it began.
    let mut rec_buf: Vec<i16> = Vec::new();
    // Last measured channel SNR, handed to the receivers for their summaries.
    let mut last_snr_db = 0.0f32;
    let mut last_lagged_seen = 0u64;
    let mut last_lag_at: Option<Instant> = None;
    // Smoothed carrier error. A single frame's centroid is noisy; a
    // calibration wants a settled figure.
    let mut freq_error_hz: Option<f64> = None;
    let mut rec_started_ms: u64 = 0;
    let mut rec_active = false;
    let mut next_call_id: u64 = 1;
    // Audio for the browser, refilled each block. Reused so a steady stream of
    // blocks does not allocate.
    let mut audio_buf: Vec<f32> = Vec::new();

    let frame_interval = Duration::from_millis(1000 / DEFAULT_FPS as u64);
    let mut last_frame = Instant::now();
    let mut peak_iq = 0.0f32;
    // Audio for the scope, decimated as it arrives and kept as a rolling
    // window so a frame always has a full trace to draw even when a block
    // lands short.
    let mut scope: VecDeque<i16> = VecDeque::with_capacity(SCOPE_SAMPLES * 2);
    let mut scope_acc = 0.0f32;
    let mut scope_n = 0.0f32;
    let mut scope_phase = 0.0f32;
    // The radio thread can die on a USB fault and take the samples with it
    // while the Fanout stays alive and silent. Without this the waterfall
    // simply freezes with no explanation.
    let mut last_block = Instant::now();
    let mut stalled = false;
    let mut last_reopen = Instant::now();
    // Recent tuner-write failures. The radio thread keeps delivering samples
    // with a wedged tuner, so IQ starvation alone never notices this: the
    // spectrum simply stops matching the frequency the UI is showing.
    // Offsets from the display centre a spur check found. Fixed relative to
    // the local oscillator, so they stay put as the radio tunes.
    let mut spurs: Vec<f64> = Vec::new();
    let mut ctl_faults: VecDeque<Instant> = VecDeque::new();
    let mut last_tune_ok = Instant::now();
    let mut fault_reopen = false;
    // A deliberate reconfigure (the LO offset moving) rather than a fault, so
    // it reopens at once instead of waiting out the fault backoff.
    let mut force_reopen_now = false;
    // Reopens that did not fix the wedge. A tuner that fails again within
    // seconds of being reopened is not going to be talked round by doing it
    // faster, and hammering it every two seconds only fills the log and keeps
    // the stream down.
    let mut futile_reopens: u32 = 0;
    let mut recover_backoff = RECOVER_INTERVAL;
    // A reopen posts a notice that only the arrival of real samples can clear.
    // Without this a tuner fault left "recovering…" on screen for good, since
    // the stall path is what used to own that message.
    let mut recovering = false;
    // Latest requested tuner state, not yet written to the hardware.
    let mut pending_tune: Option<f64> = None;
    let mut pending_gain: Option<Option<f64>> = None;
    let mut last_ctl = Instant::now() - CTL_MIN_INTERVAL;
    // What the dongle's AGC is actually set to, so an unchanged setting does
    // not cost a register write on every gain adjustment.
    let mut agc_on = cfg.gain_db.is_none();
    let mut dropped_blocks = 0u64;
    let mut lagged_blocks = 0u64;

    while !stop.load(Ordering::Relaxed) {
        // Drain commands
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                SdrCmd::Tune(f) => {
                    if (current_freq - f).abs() >= 1.0 {
                        let prev_freq = current_freq;
                        current_freq = f;
                        if (inspect_hz - prev_freq).abs() < 1.0
                            || inspect_hz
                                < current_freq
                                    - display_rate_for(current_rate, lo_offset, current_mode) * 0.49
                            || inspect_hz
                                > current_freq
                                    + display_rate_for(current_rate, lo_offset, current_mode) * 0.49
                        {
                            inspect_hz = f;
                        }
                        pending_tune = Some(f);
                        (inspect_chain, classifier) =
                            build_inspect(
                            current_rate,
                            inspect_hz - current_freq - lo_offset_for(current_rate, lo_offset, current_mode),
                            current_mode,
                        );
                        demod = Demod::new(
                            current_mode,
                            inspect_chain.fs_out(),
                            current_rate,
                            current_freq + lo_offset_for(current_rate, lo_offset, current_mode),
                            inspect_hz,
                        );
                        scope_fm = NbfmDemod::with_deviation(inspect_chain.fs_out(), 3_000.0);
                        capture
                            .lock()
                            .expect("SDR capture")
                            .clear(inspect_hz, inspect_chain.fs_out());
                        // Deliberately not stamping freq_hz here: it reports
                        // where the tuner is, and it is not there yet.
                        let mut s = status.lock().unwrap();
                        let shown = display_rate_for(current_rate, lo_offset, current_mode);
                        s.min_freq_hz = tuned_freq - shown / 2.0;
                        s.max_freq_hz = tuned_freq + shown / 2.0;
                        s.inspect_hz = inspect_hz;
                        let _ = events.send(SdrEvent::Status(s.clone()));
                    }
                }
                SdrCmd::Inspect(f) => {
                    inspect_hz = f.clamp(
                        current_freq - display_rate_for(current_rate, lo_offset, current_mode) * 0.49,
                        current_freq + display_rate_for(current_rate, lo_offset, current_mode) * 0.49,
                    );
                    (inspect_chain, classifier) =
                        build_inspect(
                            current_rate,
                            inspect_hz - current_freq - lo_offset_for(current_rate, lo_offset, current_mode),
                            current_mode,
                        );
                    demod = Demod::new(
                            current_mode,
                            inspect_chain.fs_out(),
                            current_rate,
                            current_freq + lo_offset_for(current_rate, lo_offset, current_mode),
                            inspect_hz,
                        );
                    capture
                        .lock()
                        .expect("SDR capture")
                        .clear(inspect_hz, inspect_chain.fs_out());
                    let mut s = status.lock().unwrap();
                    s.inspect_hz = inspect_hz;
                    let _ = events.send(SdrEvent::Status(s.clone()));
                }
                SdrCmd::Gain(g) => {
                    current_gain = g;
                    pending_gain = Some(g);
                    // RF power and quieting baselines are gain-dependent.
                    classifier.reset();
                    let mut s = status.lock().unwrap();
                    s.gain_db = current_gain;
                    let _ = events.send(SdrEvent::Status(s.clone()));
                }
                SdrCmd::Rate(r) => {
                    if (current_rate - r).abs() >= 1.0 {
                        current_rate = r;
                        if let Some(ref dev) = dev_opt {
                            let _ = dev.cmd.send(Cmd::Rate(r));
                        }
                        inspect_hz = inspect_hz.clamp(
                            current_freq
                                - display_rate_for(current_rate, lo_offset, current_mode) * 0.49,
                            current_freq
                                + display_rate_for(current_rate, lo_offset, current_mode) * 0.49,
                        );
                        (inspect_chain, classifier) =
                            build_inspect(
                            current_rate,
                            inspect_hz - current_freq - lo_offset_for(current_rate, lo_offset, current_mode),
                            current_mode,
                        );
                        demod = Demod::new(
                            current_mode,
                            inspect_chain.fs_out(),
                            current_rate,
                            current_freq + lo_offset_for(current_rate, lo_offset, current_mode),
                            inspect_hz,
                        );
                        scope_fm = NbfmDemod::with_deviation(inspect_chain.fs_out(), 3_000.0);
                        capture
                            .lock()
                            .expect("SDR capture")
                            .clear(inspect_hz, inspect_chain.fs_out());
                        let mut s = status.lock().unwrap();
                        s.rate_hz = current_rate;
                        let shown = display_rate_for(current_rate, lo_offset, current_mode);
                        s.min_freq_hz = current_freq - shown / 2.0;
                        s.max_freq_hz = current_freq + shown / 2.0;
                        s.inspect_hz = inspect_hz;
                        let _ = events.send(SdrEvent::Status(s.clone()));
                    }
                }
                SdrCmd::Mode(m) => {
                    if m != current_mode {
                        // Whether the LO offset applies depends on the mode's
                        // channel width, so switching to or from a wide mode
                        // moves the LO and changes the span on show.
                        let was = lo_active(current_rate, lo_offset, current_mode);
                        current_mode = m;
                        if lo_active(current_rate, lo_offset, current_mode) != was {
                            force_reopen_now = true;
                        }
                        (inspect_chain, classifier) =
                            build_inspect(
                            current_rate,
                            inspect_hz - current_freq - lo_offset_for(current_rate, lo_offset, current_mode),
                            current_mode,
                        );
                        demod = Demod::new(
                            current_mode,
                            inspect_chain.fs_out(),
                            current_rate,
                            current_freq + lo_offset_for(current_rate, lo_offset, current_mode),
                            inspect_hz,
                        );
                        scope_fm = NbfmDemod::with_deviation(inspect_chain.fs_out(), 3_000.0);
                        capture
                            .lock()
                            .expect("SDR capture")
                            .clear(inspect_hz, inspect_chain.fs_out());
                        let mut s = status.lock().unwrap();
                        s.mode = current_mode;
                        s.bandwidth_hz = f64::from(current_mode.bandwidth_hz());
                        s.lo_offset_hz = lo_offset_for(current_rate, lo_offset, current_mode);
                        let shown = display_rate_for(current_rate, lo_offset, current_mode);
                        s.min_freq_hz = current_freq - shown / 2.0;
                        s.max_freq_hz = current_freq + shown / 2.0;
                        s.audio_rate_hz = demod.audio_rate(inspect_chain.fs_out());
                        s.inspect_rate_hz = inspect_chain.fs_out();
                        let _ = events.send(SdrEvent::Status(s.clone()));
                    }
                }
                SdrCmd::Zoom(z) => {
                    let z = z.clamp(1.0, ZOOM_MAX);
                    if (z - current_zoom).abs() > 0.001 {
                        current_zoom = z;
                        // More zoom, more bins: the window narrows and the FFT
                        // grows with it, so resolution improves instead of the
                        // same bins being drawn further apart.
                        let want = (ZOOM_TARGET_BINS as f64 * current_zoom)
                            .round()
                            .clamp(256.0, 8192.0) as usize;
                        let want = want.next_power_of_two().min(8192);
                        if want != fft_size {
                            fft_size = want;
                            spectrum = Spectrum::new(fft_size);
                            pwr = Vec::with_capacity(fft_size);
                            smoothed = Vec::with_capacity(fft_size);
                        }
                        let mut s = status.lock().unwrap();
                        s.zoom = current_zoom;
                        s.fft_size = fft_size;
                        let shown = display_rate_for(current_rate, lo_offset, current_mode)
                            / current_zoom;
                        s.min_freq_hz = inspect_hz - shown / 2.0;
                        s.max_freq_hz = inspect_hz + shown / 2.0;
                        let _ = events.send(SdrEvent::Status(s.clone()));
                    }
                }
                SdrCmd::Ppm(ppm) => {
                    if let Some(ref dev) = dev_opt {
                        let _ = dev.cmd.send(Cmd::Ppm(ppm));
                    }
                    // Persist against this dongle's serial: it is a property of
                    // the crystal in it, not of the session.
                    if let Some(serial) = current_serial.clone() {
                        let mut cfg = config::load().unwrap_or_default();
                        let tuner = status.lock().unwrap().tuner.clone();
                        cfg.entry(&serial, &tuner).ppm = ppm;
                        if let Err(e) = config::save(&cfg) {
                            eprintln!("could not save ppm: {e:#}");
                        }
                    }
                    freq_error_hz = None;
                    let mut s = status.lock().unwrap();
                    s.ppm = ppm;
                    let _ = events.send(SdrEvent::Status(s.clone()));
                }
                SdrCmd::Spurs(offsets) => {
                    spurs = offsets;
                    let mut s = status.lock().unwrap();
                    s.spurs_hz = spurs.clone();
                    let _ = events.send(SdrEvent::Status(s.clone()));
                }
                SdrCmd::ClipGuard(on) => {
                    clip_guard_on = on;
                    if let Some(ref dev) = dev_opt {
                        let _ = dev.cmd.send(Cmd::ClipGuard(on));
                    }
                    let mut s = status.lock().unwrap();
                    s.clip_guard = on;
                    let _ = events.send(SdrEvent::Status(s.clone()));
                }
                SdrCmd::LoOffset(on) => {
                    if on != lo_offset {
                        lo_offset = on;
                        // The LO itself has to move, and only reopening the
                        // device re-negotiates the analog IF width that the
                        // offset needs.
                        force_reopen_now = true;
                        let mut s = status.lock().unwrap();
                        s.lo_offset = lo_offset;
                        s.lo_offset_hz = lo_offset_for(current_rate, lo_offset, current_mode);
                        // The window the offset leaves usable changes with it.
                        let shown = display_rate_for(current_rate, lo_offset, current_mode);
                        s.min_freq_hz = current_freq - shown / 2.0;
                        s.max_freq_hz = current_freq + shown / 2.0;
                        let _ = events.send(SdrEvent::Status(s.clone()));
                    }
                }
                SdrCmd::SwitchDevice(new_serial) => {
                    if current_serial.as_deref() != Some(&new_serial) {
                        rx_opt = None;
                        dev_opt = None;
                        match open_sdr(
                            Some(&new_serial),
                            current_freq,
                            current_rate,
                            current_gain,
                            lo_offset_for(current_rate, lo_offset, current_mode),
                        )
                        {
                            Ok((new_dev, s_serial, s_tuner)) => {
                                current_serial = Some(s_serial.clone());
                                let new_rx = new_dev.iq.subscribe_with_depth(16);
                                dev_opt = Some(new_dev);
                                rx_opt = Some(new_rx);
                                let ppm = config::load().unwrap_or_default().ppm_for(&s_serial);
                                (inspect_chain, classifier) =
                                    build_inspect(
                            current_rate,
                            inspect_hz - current_freq - lo_offset_for(current_rate, lo_offset, current_mode),
                            current_mode,
                        );
                                demod = Demod::new(
                            current_mode,
                            inspect_chain.fs_out(),
                            current_rate,
                            current_freq + lo_offset_for(current_rate, lo_offset, current_mode),
                            inspect_hz,
                        );
                        scope_fm = NbfmDemod::with_deviation(inspect_chain.fs_out(), 3_000.0);
                                capture
                                    .lock()
                                    .expect("SDR capture")
                                    .clear(inspect_hz, inspect_chain.fs_out());
                                let mut s = status.lock().unwrap();
                                s.serial = s_serial;
                                s.tuner = s_tuner;
                                s.ppm = ppm;
                                s.inspect_hz = inspect_hz;
                                let _ = events.send(SdrEvent::Status(s.clone()));
                            }
                            Err(e) => {
                                eprintln!("failed to switch to device {new_serial}: {e:#}");
                            }
                        }
                    }
                }
            }
        }

        // Apply at most one queued tuner write per interval, carrying whatever
        // the latest request was. A burst of clicks or a dragged slider is one
        // write, not thirty, which is the difference between a tuner that
        // keeps up and one that stalls its control endpoint.
        if last_ctl.elapsed() >= CTL_MIN_INTERVAL
            && (pending_tune.is_some() || pending_gain.is_some())
        {
            if let Some(ref dev) = dev_opt {
                if let Some(f) = pending_tune.take() {
                    let _ = dev
                        .cmd
                        .send(Cmd::Tune(f + lo_offset_for(current_rate, lo_offset, current_mode)));
                }
                if let Some(g) = pending_gain.take() {
                    match g {
                        Some(db) => {
                            if agc_on {
                                let _ = dev.cmd.send(Cmd::Agc(false));
                                agc_on = false;
                            }
                            let _ = dev.cmd.send(Cmd::Gain(db));
                        }
                        None => {
                            if !agc_on {
                                let _ = dev.cmd.send(Cmd::Agc(true));
                                agc_on = true;
                            }
                        }
                    }
                }
                last_ctl = Instant::now();
            } else {
                // No radio to write to: drop the request rather than replaying
                // a stale tuning at whatever reopens next.
                pending_tune = None;
                pending_gain = None;
            }
        }

        // An I2C write that fails with -9 leaves the dongle's control endpoint
        // stalled: every later tune and gain write fails too, the radio thread
        // gives up, and the Fanout stays alive and silent. The stall does not
        // clear itself, and reopening the device is what resets it — which is
        // why restarting the process "fixed" it. Do that here instead, so a
        // USB fault costs a second of waterfall rather than the session.
        if force_reopen_now || ((stalled || fault_reopen) && last_reopen.elapsed() >= recover_backoff)
        {
            let wedged = fault_reopen;
            let deliberate = force_reopen_now;
            if wedged {
                // Wedging again soon after a reopen means the reopen achieved
                // nothing; back off rather than spin.
                if last_reopen.elapsed() < recover_backoff * 4 {
                    futile_reopens = futile_reopens.saturating_add(1);
                    recover_backoff = (recover_backoff * 2).min(RECOVER_INTERVAL_MAX);
                } else {
                    futile_reopens = 0;
                    recover_backoff = RECOVER_INTERVAL;
                }
            }
            force_reopen_now = false;
            fault_reopen = false;
            ctl_faults.clear();
            last_reopen = Instant::now();
            rx_opt = None;
            dev_opt = None;
            match open_sdr(
            current_serial.as_deref(),
            current_freq,
            current_rate,
            current_gain,
            lo_offset_for(current_rate, lo_offset, current_mode),
        ) {
                Ok((new_dev, s_serial, s_tuner)) => {
                    let _ = new_dev.cmd.send(Cmd::ClipGuard(clip_guard_on));
                    // open() tunes as it goes, so the reopened device is
                    // genuinely where it was asked to be.
                    tuned_freq = current_freq;
                    last_tune_ok = Instant::now();
                    let new_rx = new_dev.iq.subscribe_with_depth(16);
                    dev_opt = Some(new_dev);
                    rx_opt = Some(new_rx);
                    let ppm = config::load().unwrap_or_default().ppm_for(&s_serial);
                    current_serial = Some(s_serial.clone());
                    // The reopened dongle is a fresh chain: anything buffered
                    // from before the fault was taken at an unknown tuning.
                    (inspect_chain, classifier) =
                        build_inspect(
                            current_rate,
                            inspect_hz - current_freq - lo_offset_for(current_rate, lo_offset, current_mode),
                            current_mode,
                        );
                    demod = Demod::new(
                            current_mode,
                            inspect_chain.fs_out(),
                            current_rate,
                            current_freq + lo_offset_for(current_rate, lo_offset, current_mode),
                            inspect_hz,
                        );
                    capture
                        .lock()
                        .expect("SDR capture")
                        .clear(inspect_hz, inspect_chain.fs_out());
                    if deliberate {
                        eprintln!("SDR {s_serial}: reopened to change the LO offset");
                    } else {
                        eprintln!(
                            "SDR {s_serial}: reopened after {}",
                            if wedged { "a wedged tuner" } else { "a USB fault" }
                        );
                    }
                    let mut s = status.lock().unwrap();
                    s.serial = s_serial;
                    s.tuner = s_tuner;
                    s.ppm = ppm;
                    if !deliberate {
                        s.error = Some(if !wedged {
                            "recovering from a USB fault…".into()
                        } else if futile_reopens >= 3 {
                            // Reopening has been tried and did not take. This
                            // is the dongle, not the software: it needs power
                            // removed, and often a cool-down.
                            format!(
                                "tuner will not retune after {futile_reopens} reopens — \
                                 unplug the dongle and let it cool"
                            )
                        } else {
                            "tuner stopped responding — reopening…".into()
                        });
                        recovering = true;
                    }
                    let _ = events.send(SdrEvent::Status(s.clone()));
                }
                Err(e) => {
                    eprintln!("SDR: reopen failed: {e:#}");
                    let mut s = status.lock().unwrap();
                    s.error = Some(format!("radio offline: {e}"));
                    let _ = events.send(SdrEvent::Status(s.clone()));
                }
            }
        }

        let (Some(ref dev), Some(ref rx)) = (dev_opt.as_ref(), rx_opt.as_ref()) else {
            std::thread::sleep(Duration::from_millis(25));
            continue;
        };

        while let Ok(line) = dev.log.try_recv() {
            if line.contains("failed") {
                let now = Instant::now();
                while ctl_faults
                    .front()
                    .is_some_and(|t| now.duration_since(*t) > CTL_FAULT_WINDOW)
                {
                    ctl_faults.pop_front();
                }
                ctl_faults.push_back(now);
                if ctl_faults.len() >= CTL_FAULT_LIMIT
                    && last_tune_ok.elapsed() >= CTL_QUIET_BEFORE_REOPEN
                {
                    ctl_faults.clear();
                    fault_reopen = true;
                }
                if line.contains("tune failed") {
                    // The tuner stayed where it was. Correct the status so the
                    // display stops claiming a frequency the radio is not on.
                    let shown = display_rate_for(current_rate, lo_offset, current_mode);
                    let mut st = status.lock().unwrap();
                    st.freq_hz = tuned_freq;
                    st.min_freq_hz = tuned_freq - shown / 2.0;
                    st.max_freq_hz = tuned_freq + shown / 2.0;
                    st.error = Some(format!(
                        "tuner did not accept {:.4} MHz — still on {:.4}",
                        current_freq / 1e6,
                        tuned_freq / 1e6
                    ));
                    let _ = events.send(SdrEvent::Status(st.clone()));
                }
            }
            eprintln!(
                "SDR {}: {line}",
                status.lock().map(|s| s.serial.clone()).unwrap_or_default()
            );
        }
        while let Ok(ev) = dev.events.try_recv() {
            match ev {
                scannerd_radio::Event::StreamStats {
                    dropped_blocks: d,
                    lagged_deliveries: l,
                    ..
                } => {
                    dropped_blocks = d;
                    lagged_blocks = l;
                }
                // Only published after a tune the driver accepted, so this is
                // where the radio really is.
                scannerd_radio::Event::State(state) => {
                    // The driver reports where the *local oscillator* is. With
                    // the LO parked off-centre that is not the middle of the
                    // window on screen, and taking it as such shifted the whole
                    // frequency axis by the offset — so a click landed a
                    // few hundred kHz from the passband it produced.
                    let lo_off = lo_offset_for(current_rate, lo_offset, current_mode);
                    if let Some(hw) = state.frequency
                        && (hw - lo_off - tuned_freq).abs() >= 1.0
                    {
                        // A tune the driver accepted: the radio is healthy.
                        futile_reopens = 0;
                        recover_backoff = RECOVER_INTERVAL;
                        last_tune_ok = Instant::now();
                        ctl_faults.clear();
                        tuned_freq = hw - lo_off;
                        inspect_chain.set_offset(inspect_hz - hw);
                        // These extract their own channel out of the raw span,
                        // so they want the oscillator, not the display centre.
                        match &mut demod {
                            Demod::P25 { rx } => rx.retune(hw),
                            Demod::Dmr { rx } => rx.retune(hw),
                            _ => {}
                        }
                        let shown = display_rate_for(current_rate, lo_offset, current_mode);
                        let mut st = status.lock().unwrap();
                        st.freq_hz = tuned_freq;
                        st.min_freq_hz = tuned_freq - shown / 2.0;
                        st.max_freq_hz = tuned_freq + shown / 2.0;
                        let _ = events.send(SdrEvent::Status(st.clone()));
                    }
                }
                _ => {}
            }
        }
        let Ok(block) = rx.recv_timeout(Duration::from_millis(20)) else {
            if !stalled && last_block.elapsed() >= Duration::from_secs(3) {
                stalled = true;
                let why = format!(
                    "no IQ for {:.0}s - radio thread stopped",
                    last_block.elapsed().as_secs_f32()
                );
                eprintln!("SDR: {why}");
                let mut s = status.lock().unwrap();
                s.error = Some(why);
                let _ = events.send(SdrEvent::Status(s.clone()));
            }
            continue;
        };
        if stalled || recovering {
            stalled = false;
            recovering = false;
            let mut s = status.lock().unwrap();
            s.error = None;
            let _ = events.send(SdrEvent::Status(s.clone()));
        }
        last_block = Instant::now();

        for c in block.iter() {
            let mag = c.re.abs().max(c.im.abs());
            if mag > peak_iq {
                peak_iq = mag;
            }
        }

        // DC offset, LO leakage and the IQ image are removed here, before the
        // spectrum measures the span or the inspect chain mixes out of it. The
        // block is shared with other subscribers, so correct a copy.
        clean.clear();
        clean.extend_from_slice(&block);
        front.process(&mut clean);

        spectrum.power_dbfs(&clean, &mut pwr);

        // Process channel for classifier
        inspect_iq.clear();
        inspect_chain.process(&clean, &mut inspect_iq);
        let fs_chain = inspect_chain.fs_out();

        // A spur inside the channel corrupts the decode, not just the picture.
        // The inspect chain has already mixed the channel to DC, so a spur at
        // display offset `s` lands at `s - channel_offset` here.
        if !spurs.is_empty() && !inspect_iq.is_empty() {
            let channel_off = inspect_hz - current_freq;
            let half_bw = f64::from(current_mode.bandwidth_hz()) / 2.0;
            for &spur in &spurs {
                let rel = spur - channel_off;
                if rel.abs() < half_bw && rel.abs() > 100.0 {
                    let mut notch = ChannelNotch::new(rel, fs_chain, 4_000.0);
                    notch.process(&mut inspect_iq, &mut notch_scratch);
                }
            }
        }

        audio_buf.clear();

        // The classifier is the demodulator for every narrowband mode: it
        // already produces the voice and discriminator the decoders need, so
        // running it is not an extra cost. WFM has nothing it can use.
        let classification = match current_mode {
            SdrMode::Nfm | SdrMode::Packet => {
                let c = classifier.process(&inspect_iq, inspect_hz);
                audio_buf.extend_from_slice(classifier.monitor_voice());
                {
                    let mut capture = capture.lock().expect("SDR capture");
                    capture.append(
                        inspect_hz,
                        fs_chain,
                        &inspect_iq,
                        classifier.monitor_voice(),
                        classifier.monitor_discriminator_hz(),
                    );
                }
                c
            }
            SdrMode::Auto => {
                // Run everything. The classifier identifies and supplies the
                // discriminator the packet decoders read; the two voice
                // receivers work the same channel in parallel, and whichever
                // one locks is what gets believed and heard.
                let c = classifier.process(&inspect_iq, inspect_hz);
                {
                    let mut capture = capture.lock().expect("SDR capture");
                    capture.append(
                        inspect_hz,
                        fs_chain,
                        &inspect_iq,
                        classifier.monitor_voice(),
                        classifier.monitor_discriminator_hz(),
                    );
                }
                let mut p25_state = (false, false);
                let mut dmr_state = (false, false);
                if let Demod::Auto { p25, dmr, .. } = &mut demod {
                    if let Some(ev) = p25.process(&clean, last_snr_db) {
                        let _ = events.send(SdrEvent::Decode {
                            inspect_hz,
                            event: call_event(SdrMode::P25, &ev),
                        });
                    }
                    if let Some(ev) = dmr.process(&clean, last_snr_db) {
                        let _ = events.send(SdrEvent::Decode {
                            inspect_hz,
                            event: call_event(SdrMode::Dmr, &ev),
                        });
                    }
                    p25_state = (p25.locked(), p25.in_call());
                    dmr_state = (dmr.locked(), dmr.in_call());
                    // Only one of them can be right about the audio.
                    if p25.in_call() {
                        audio_buf.extend_from_slice(p25.audio());
                    } else if dmr.in_call() {
                        audio_buf.extend_from_slice(dmr.audio());
                    } else {
                        audio_buf.extend_from_slice(classifier.monitor_voice());
                    }
                }
                if let Demod::Auto {
                    aprs,
                    pocsag,
                    flex,
                    ..
                } = &mut demod
                {
                    let disc = classifier.monitor_discriminator_hz();
                    for packet in aprs.process(disc) {
                        let _ = events.send(SdrEvent::Decode {
                            inspect_hz,
                            event: packet_event(&packet),
                        });
                    }
                    for dec in pocsag.iter_mut() {
                        for msg in dec.process(disc) {
                            let _ = events.send(SdrEvent::Decode {
                                inspect_hz,
                                event: pocsag_event(&msg),
                            });
                        }
                    }
                    for msg in flex.process(disc) {
                        let _ = events.send(SdrEvent::Decode {
                            inspect_hz,
                            event: flex_event(&msg),
                        });
                    }
                }
                // A receiver that has locked knows more than a heuristic, so
                // it wins the label; otherwise the classifier's answer stands.
                if p25_state.0 || dmr_state.0 {
                    let is_p25 = p25_state.0;
                    ClassificationResult {
                        active: p25_state.1 || dmr_state.1 || c.active,
                        protocol: if is_p25 { "P25 Phase 1" } else { "DMR Tier II" }.into(),
                        modulation: if is_p25 { "C4FM @ 4800 Bd" } else { "4-FSK @ 4800 Bd" }
                            .into(),
                        details: Some(
                            if p25_state.1 || dmr_state.1 {
                                "voice call"
                            } else {
                                "locked, idle"
                            }
                            .into(),
                        ),
                        confidence: 0.95,
                        ..c
                    }
                } else {
                    c
                }
            }
            SdrMode::P25 | SdrMode::Dmr => {
                // These receivers extract and equalise their own channel out
                // of the span, so they are fed the corrected span rather than
                // the inspect chain's narrowband output.
                // The receivers use this only for the call summary, but a
                // real figure is more use in the log than a hardcoded zero.
                let snr_hint = last_snr_db;
                let (event, locked, in_call) = match &mut demod {
                    Demod::P25 { rx } => {
                        let ev = rx.process(&clean, snr_hint);
                        audio_buf.extend_from_slice(rx.audio());
                        (ev, rx.locked(), rx.in_call())
                    }
                    Demod::Dmr { rx } => {
                        let ev = rx.process(&clean, snr_hint);
                        audio_buf.extend_from_slice(rx.audio());
                        (ev, rx.locked(), rx.in_call())
                    }
                    _ => (None, false, false),
                };

                // Keep the call's audio so it can be replayed. Started arrives
                // on the same pass that produced the first audio, so the reset
                // has to happen before this pass is appended.
                if matches!(event, Some(CallEvent::Started)) {
                    rec_buf.clear();
                    rec_started_ms = now_ms();
                    rec_active = true;
                }
                if rec_active {
                    rec_buf.extend(
                        audio_buf
                            .iter()
                            .map(|&x| (x.clamp(-1.0, 1.0) * 28_000.0) as i16),
                    );
                }
                if let Some(CallEvent::Ended(summary)) = &event
                    && rec_active
                {
                    rec_active = false;
                    let digital = summary.digital.as_ref();
                    let peak = rec_buf
                        .iter()
                        .map(|&v| (v as f32 / 32768.0).abs())
                        .fold(0.0f32, f32::max);
                    let record = RecordedCall {
                        id: next_call_id,
                        protocol: if current_mode == SdrMode::P25 { "P25" } else { "DMR" }
                            .into(),
                        started_ms: rec_started_ms,
                        duration_s: summary.duration_s(),
                        freq_hz: inspect_hz,
                        rate_hz: demod.audio_rate(fs_chain).round() as u32,
                        samples: rec_buf.len(),
                        peak,
                        encrypted: digital.is_some_and(|d| d.encrypted),
                        decrypted: digital.is_some_and(|d| d.decrypted),
                        algorithm: digital
                            .and_then(|d| d.algorithm_id)
                            .map(|a| format!("0x{a:02X}")),
                        tone: summary.tone.as_ref().map(|t| t.label()),
                        source_id: digital.and_then(|d| d.source_id),
                        target_id: digital.and_then(|d| d.target_id),
                        audio: std::mem::take(&mut rec_buf),
                    };
                    next_call_id += 1;
                    let mut ring = calls.lock().expect("calls");
                    if ring.len() >= RECENT_CALLS {
                        ring.pop_front();
                    }
                    ring.push_back(record);
                }
                if let Some(ev) = event {
                    let _ = events.send(SdrEvent::Decode {
                        inspect_hz,
                        event: call_event(current_mode, &ev),
                    });
                }
                ClassificationResult {
                    active: in_call,
                    protocol: if current_mode == SdrMode::P25 {
                        "P25 Phase 1".into()
                    } else {
                        "DMR Tier II".into()
                    },
                    modulation: if current_mode == SdrMode::P25 {
                        "C4FM @ 4800 Bd".into()
                    } else {
                        "4-FSK @ 4800 Bd".into()
                    },
                    details: Some(if in_call {
                        "voice call".into()
                    } else if locked {
                        "locked, idle".into()
                    } else {
                        "searching for sync".into()
                    }),
                    confidence: if locked { 0.95 } else { 0.0 },
                    ..Default::default()
                }
            }
            SdrMode::Wfm => {
                if let Demod::Wfm {
                    fm,
                    disc,
                    deemph,
                    decim,
                    phase,
                    acc,
                } = &mut demod
                {
                    // Instantaneous frequency in Hz, scaled by the deviation
                    // so full deflection is full scale, then de-emphasised and
                    // box-averaged down to the audio rate.
                    fm.discriminator_hz(&inspect_iq, disc);
                    for &hz in disc.iter() {
                        let v = deemph.process(hz / WFM_DEVIATION_HZ);
                        *acc += v;
                        *phase += 1;
                        if *phase >= *decim {
                            audio_buf.push((*acc / *decim as f32).clamp(-1.0, 1.0));
                            *acc = 0.0;
                            *phase = 0;
                        }
                    }
                    // Deliberately not captured. The rolling buffer holds
                    // I/Q, discriminator and audio at one rate, and wideband
                    // FM has the first two at 256 kHz and the audio at 51.2 —
                    // storing them together would label the voice WAV at five
                    // times its real rate. The I/Q would be useless for replay
                    // regardless: 256 kHz is outside the range the classifier
                    // accepts, and none of the digital decoders apply here.
                }
                ClassificationResult {
                    active: true,
                    protocol: "WFM broadcast".into(),
                    modulation: "Wideband FM".into(),
                    ..Default::default()
                }
            }
        };

        // Packet runs every decoder over the same discriminator the classifier
        // built, so whatever is in the passband gets decoded without being
        // told in advance which format it is.
        if let Demod::Packet {
            aprs,
            pocsag,
            flex,
        } = &mut demod
        {
            let disc = classifier.monitor_discriminator_hz();
            for packet in aprs.process(disc) {
                let _ = events.send(SdrEvent::Decode {
                    inspect_hz,
                    event: packet_event(&packet),
                });
            }
            for dec in pocsag.iter_mut() {
                for msg in dec.process(disc) {
                    let _ = events.send(SdrEvent::Decode {
                        inspect_hz,
                        event: pocsag_event(&msg),
                    });
                }
            }
            for msg in flex.process(disc) {
                let _ = events.send(SdrEvent::Decode {
                    inspect_hz,
                    event: flex_event(&msg),
                });
            }
        }

        // The scope shows whatever the mode is actually working with.
        scope_src.clear();
        scope_src_rate = 0.0;
        match current_mode {
            SdrMode::P25 | SdrMode::Dmr => {
                // Discriminator at the channel rate: 10 samples a symbol, which
                // is what an eye diagram is drawn from.
                scope_fm.discriminator_hz(&inspect_iq, &mut scope_disc);
                scope_src.extend(scope_disc.iter().map(|&hz| (hz / 3_000.0).clamp(-1.0, 1.0)));
                scope_src_rate = fs_chain;
                scope_dev_scale = 3_000.0;
            }
            SdrMode::Wfm => {
                if let Demod::Wfm { disc, .. } = &demod {
                    // Pre-de-emphasis, so the pilot and subcarriers survive.
                    scope_src
                        .extend(disc.iter().map(|&hz| (hz / WFM_DEVIATION_HZ).clamp(-1.0, 1.0)));
                    scope_src_rate = WFM_IF_RATE;
                    scope_dev_scale = WFM_DEVIATION_HZ;
                }
            }
            _ => {
                scope_src.extend_from_slice(&audio_buf);
                scope_src_rate = demod.audio_rate(fs_chain);
                scope_dev_scale = 0.0;
            }
        }

        let audio_rate = demod.audio_rate(fs_chain);
        if !audio_buf.is_empty() && audio_rate > 0.0 {
            let _ = audio_tx.send(AudioFrame {
                rate_hz: audio_rate.round() as u32,
                samples: audio_buf
                    .iter()
                    .map(|&x| (x.clamp(-1.0, 1.0) * 28_000.0) as i16)
                    .collect(),
            });
        }
        // Box-average down to SCOPE_RATE_HZ rather than picking every Nth
        // sample: a plain decimation would alias voice energy straight into
        // the scope's own frequency axis.
        {
            let fs_in = scope_src_rate as f32;
            let scope_rate = current_mode.scope_rate_hz().min(fs_in.max(1.0));
            let step = if fs_in > 0.0 { scope_rate / fs_in } else { 0.0 };
            if step > 0.0 {
                for &v in scope_src.iter() {
                    scope_acc += v;
                    scope_n += 1.0;
                    scope_phase += step;
                    if scope_phase >= 1.0 {
                        scope_phase -= 1.0;
                        let mean = if scope_n > 0.0 { scope_acc / scope_n } else { 0.0 };
                        scope.push_back((mean.clamp(-1.0, 1.0) * 32_000.0) as i16);
                        scope_acc = 0.0;
                        scope_n = 0.0;
                        if scope.len() > SCOPE_SAMPLES {
                            scope.pop_front();
                        }
                    }
                }
            }
        }

        for event in classifier.take_decode_events() {
            let _ = events.send(SdrEvent::Decode { inspect_hz, event });
        }

        if last_frame.elapsed() >= frame_interval && !pwr.is_empty() {
            last_frame = Instant::now();
            if let Ok(mut s) = status.lock() {
                s.inspect_rate_hz = inspect_chain.fs_out();
                s.dropped_blocks = dropped_blocks;
                s.lagged_blocks = lagged_blocks;
                s.freq_error_hz = freq_error_hz;
                // Running every decoder at once costs about a core. If the
                // span is wide enough that blocks are being dropped, the
                // operator should hear it from the receiver rather than
                // wonder why decodes are patchy.
                // Lag comes in bursts, so a frame-by-frame test flickers the
                // message on and off. Hold it for a few seconds past the last
                // dropped block and clear it only once the receiver has been
                // keeping up for a while.
                if lagged_blocks > last_lagged_seen {
                    last_lagged_seen = lagged_blocks;
                    last_lag_at = Some(Instant::now());
                }
                let lagging = last_lag_at.is_some_and(|t| t.elapsed() < LAG_WARN_HOLD);
                if current_mode == SdrMode::Auto && lagging {
                    s.error = Some(format!(
                        "AUTO cannot keep up at {:.3} MS/s — dropping blocks; \
                         try a narrower span",
                        current_rate / 1e6
                    ));
                } else if s
                    .error
                    .as_deref()
                    .is_some_and(|e| e.starts_with("AUTO cannot keep up"))
                {
                    s.error = None;
                }
            }
            smooth_bins(&pwr, 3, &mut smoothed);

            // The FFT covers the sampled span around the *hardware* centre.
            // With the LO parked off-centre that is not the window being
            // shown, so cut out the part that is: the bins either side of the
            // requested frequency that stay inside the tuner's passband.
            let lo_off = lo_offset_for(current_rate, lo_offset, current_mode);
            let full_rate = display_rate_for(current_rate, lo_offset, current_mode);
            let shown_rate = full_rate / current_zoom;
            // Zoom in on what is being received, not on the middle of the
            // span: the operator is looking at a signal, not at a coordinate.
            let want_centre = if current_zoom > 1.0 { inspect_hz } else { tuned_freq };
            // Keep the window inside the part of the span that is usable.
            let limit = full_rate / 2.0 - shown_rate / 2.0;
            let centre_off = (want_centre - tuned_freq).clamp(-limit.max(0.0), limit.max(0.0));
            let shown_centre = tuned_freq + centre_off;
            let n = smoothed.len();
            let bin_of = |offset_hz: f64| -> usize {
                let f = (offset_hz / current_rate + 0.5) * n as f64;
                f.round().clamp(0.0, n as f64 - 1.0) as usize
            };
            let lo_bin = bin_of(centre_off - lo_off - shown_rate / 2.0);
            let hi_bin = bin_of(centre_off - lo_off + shown_rate / 2.0).max(lo_bin + 1);
            let mut shown_buf = smoothed[lo_bin..hi_bin.min(n)].to_vec();
            // Guard scales with the bin width so the notch stays a few bins
            // wide whatever the span and FFT size are.
            let guard_hz = (shown_rate / shown_buf.len().max(1) as f64 * 3.0).max(4_000.0);
            notch_display(&mut shown_buf, shown_rate, &spurs, guard_hz);
            let shown: &[f32] = &shown_buf;

            let mut min_db = f32::INFINITY;
            let mut max_db = f32::NEG_INFINITY;
            for &x in shown {
                if x < min_db {
                    min_db = x;
                }
                if x > max_db {
                    max_db = x;
                }
            }

            // Where the hardware's own artefacts land in display coordinates:
            // its DC/LO leakage, and the ±fs/4 images either side of it.
            let spurs = [
                lo_off,
                lo_off + current_rate / 4.0,
                lo_off - current_rate / 4.0,
            ];
            let (channel_dbfs, noise_dbfs) = channel_level(
                shown,
                shown_rate,
                inspect_hz - shown_centre,
                f64::from(current_mode.bandwidth_hz()),
            );
            last_snr_db = channel_dbfs - noise_dbfs;
            // Only meaningful when something is actually there to measure.
            if channel_dbfs - noise_dbfs > 6.0
                && let Some(err) = carrier_offset_hz(
                    shown,
                    shown_rate,
                    inspect_hz - shown_centre,
                    f64::from(current_mode.bandwidth_hz()),
                )
            {
                freq_error_hz = Some(match freq_error_hz {
                    Some(prev) => prev + (err - prev) * 0.15,
                    None => err,
                });
            } else if last_snr_db < 3.0 {
                freq_error_hz = None;
            }
            let peaks = find_peaks(shown, tuned_freq, shown_rate, &spurs);
            let mut classification = classification;
            // WFM, P25 and DMR bypass the classifier, so its own measurements
            // are left at their defaults and the signal readout showed zeros.
            // Fill them from what was actually measured this frame.
            if current_mode != SdrMode::Nfm && current_mode != SdrMode::Packet {
                classification.rf_dbfs = channel_dbfs;
                classification.snr_db = channel_dbfs - noise_dbfs;
                if scope_dev_scale > 0.0 && !scope_src.is_empty() {
                    let mut peak = 0.0f32;
                    let mut sumsq = 0.0f64;
                    let mut sum = 0.0f64;
                    for &v in &scope_src {
                        peak = peak.max(v.abs());
                        sum += f64::from(v);
                        sumsq += f64::from(v) * f64::from(v);
                    }
                    let n = scope_src.len() as f64;
                    let rms = (sumsq / n).sqrt() as f32;
                    classification.peak_dev_hz = peak * scope_dev_scale;
                    classification.rms_dev_hz = rms * scope_dev_scale;
                    // The mean of an FM discriminator is the carrier's offset
                    // from where the receiver thinks it is — which is exactly
                    // the measurement a frequency calibration needs.
                    classification.center_offset_hz = (sum / n) as f32 * scope_dev_scale;
                }
            }
            if classification.active
                && let Some(peak) = peaks
                    .iter()
                    .filter(|p| (p.freq_hz - inspect_hz).abs() <= 15_000.0)
                    .min_by(|a, b| {
                        (a.freq_hz - inspect_hz)
                            .abs()
                            .total_cmp(&(b.freq_hz - inspect_hz).abs())
                    })
            {
                classification.snr_db = peak.snr_db;
            }

            let _ = events.send(SdrEvent::Fft {
                center_hz: shown_centre,
                rate_hz: shown_rate,
                min_db,
                max_db,
                pwr: shown.to_vec(),
                peak_iq,
                inspect_hz,
                classification: Some(classification),
                peaks,
                scope: scope.iter().copied().collect(),
                scope_rate_hz: current_mode.scope_rate_hz(),
                scope_kind: current_mode.scope_kind(),
                symbol_rate_hz: current_mode.symbol_rate_hz(),
                channel_dbfs,
                noise_dbfs,
            });

            peak_iq = 0.0;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_iq_capture_replays_through_a_fresh_classifier() {
        let mut samples = Vec::new();
        for i in 0..4800 {
            let phase = std::f32::consts::TAU * 200.0 * i as f32 / INSPECT_RATE as f32;
            samples.extend([
                (phase.cos() * 20_000.0) as i16,
                (phase.sin() * 20_000.0) as i16,
            ]);
        }
        let wav = pcm_wav(&samples, INSPECT_RATE as u32, 2);
        let replay = replay_iq_wav(&wav, 155_000_000.0).unwrap();
        assert_eq!(replay.sample_rate_hz, 48_000);
        assert_eq!(replay.samples, 4800);
        assert_eq!(replay.classification.protocol, "Carrier / Beacon");
    }

    /// Every span the UI offers must produce a classifier clocked at the rate
    /// the chain really delivers. Telling it a nominal 48 kHz was a 0.8-6.7%
    /// error, which is 15-130x the timing budget of a POCSAG batch.
    #[test]
    fn the_classifier_is_clocked_from_the_chain_not_the_nominal_rate() {
        for span in [
            2_048_000.0f64,
            1_800_000.0,
            1_536_000.0,
            1_024_000.0,
            512_000.0,
            256_000.0,
        ] {
            let (chain, _) = build_inspect(span, 0.0, SdrMode::Nfm);
            let decim = (span / INSPECT_RATE).round();
            assert_eq!(chain.fs_out(), span / decim, "span {span}");
        }
        // 1.536 MHz is the only one that lands on the nominal rate; the point
        // of the fix is the other five.
        let (exact, _) = build_inspect(1_536_000.0, 0.0, SdrMode::Nfm);
        assert_eq!(exact.fs_out(), INSPECT_RATE);
        let (awkward, _) = build_inspect(2_048_000.0, 0.0, SdrMode::Nfm);
        assert!((awkward.fs_out() - INSPECT_RATE).abs() > 300.0);
    }

    /// A capture is stamped with the rate it was taken at, so a replay
    /// reconstructs the same clock the live classifier had.
    #[test]
    fn a_capture_is_stamped_with_the_rate_it_was_taken_at() {
        let mut buf = CaptureBuffer::default();
        buf.clear(155_000_000.0, 47_627.906_976_744_19);
        assert_eq!(buf.rate(), 47_628);
        let wav = pcm_wav(&[0i16; 8], buf.rate(), 2);
        assert_eq!(
            u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
            47_628
        );
        let replay = replay_iq_wav(&wav, 155_000_000.0).unwrap();
        assert_eq!(replay.sample_rate_hz, 47_628);
    }

    #[test]
    fn replay_rejects_voice_wav_as_iq() {
        let wav = pcm_wav(&[0; 128], INSPECT_RATE as u32, 1);
        assert!(replay_iq_wav(&wav, 155_000_000.0).is_err());
    }
}
