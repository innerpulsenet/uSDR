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
use scannerd_engine::rds::RdsDecoder;
use scannerd_engine::stereo::{StereoStatus, WfmAudio};
use scannerd_engine::{CallEvent, ToneCode};
use scannerd_engine::leveler::Leveler;
pub use scannerd_engine::passband::FilterShape;
use scannerd_engine::noisegate::NoiseGate;
use scannerd_engine::autonotch::AutoNotch;
use scannerd_engine::agc::AgcSpeed;
use scannerd_engine::denoise::{Denoise, NrLevel};
use scannerd_engine::ssb::{SSB_HIGH_HZ, SSB_LOW_HZ, Sideband, SsbDemod};
use scannerd_engine::nbfm::{Demodulated, NbfmDemod};
use scannerd_engine::nxdn::{NXDN_BANDWIDTH_HZ, NxdnChannelReceiver, NxdnSpec};
use scannerd_engine::{ClassificationResult, DecodeEvent, SignalClassifier};
use scannerd_radio::{Cmd, Device, DeviceConfig, Role, device};
use crate::devices as config;
use crate::scan::{DigitalEvidence, Scanner, Signal as ScanSignal};
use crate::scan::ScanMode;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

pub const DEFAULT_FREQ_HZ: f64 = 155_000_000.0;
pub const DEFAULT_RATE_HZ: f64 = 2_048_000.0;
pub const DEFAULT_FFT_SIZE: usize = 1024;
pub const DEFAULT_FPS: u32 = 25;
pub const INSPECT_RATE: f64 = 48_000.0;
/// Packet/FLEX 6400 4-FSK wants ≥4 samples/symbol at 3200 sym/s; 96 kS/s
/// gives 15, the same rate the pager walking bank uses. Voice inspect stays
/// at 48 kHz.
pub const PACKET_INSPECT_RATE: f64 = 96_000.0;
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
const CTL_FAULT_WINDOW: Duration = Duration::from_secs(15);
const CTL_FAULT_LIMIT: usize = 2;
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

/// Optional receive processing, chosen by the operator and saved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Processing {
    /// Spectral noise reduction on monitored voice: NFM, AM, USB and LSB.
    pub nr: NrLevel,
    /// The heterodyne-hunting auto-notch on the same voice modes. NFM has
    /// always had it; it is on by default.
    pub auto_notch: bool,
    /// The wideband impulse blanker, 0 (off) to 3 (aggressive). It clips the
    /// leading edge of strong narrowband bursts, so it stays off unless the
    /// operator is fighting ignition or power-line noise.
    pub blanker: u8,
    /// AM and sideband AGC speed.
    pub agc: AgcSpeed,
}

impl Default for Processing {
    fn default() -> Self {
        Self { nr: NrLevel::Off, auto_notch: true, blanker: 0, agc: AgcSpeed::Medium }
    }
}

/// The broadcast audio band, shared with receiver B's mono WFM.
pub(crate) use scannerd_engine::stereo::wfm_audio_filter;
/// Pager FSK needs more acquisition width than voice: FLEX's outer level is
/// about ±4.8 kHz and the AFC can inherit ±6.5 kHz of RTL crystal error. A
/// 15 kHz filter clipped that signal before the AFC or decoder could measure
/// it. multimon-ng is normally fed a 22.05 kHz discriminator stream; passing
/// 25 kHz here gives equivalent acquisition room while still rejecting the
/// adjacent 25 kHz channel centre.
pub const PACKET_BANDWIDTH_HZ: f32 = 25_000.0;

/// Half-width the PAGER sweep covers either side of the tuned frequency.
/// The 929/931 MHz paging plans span about a megahertz; ±480 kHz at a 25 kHz
/// raster is 39 channels, all inside a 2.048 MHz span's usable half (~920 kHz).
pub const PAGER_HALF_BAND_HZ: f64 = 480_000.0;
/// AIS's two channels and their filters, 161.96 to 162.04 MHz: the LO
/// plan parks DC outside it.
const AIS_BANDWIDTH_HZ: f32 = 80_000.0;

/// AM channel width. Aviation voice is allocated 8.5 kHz (25 kHz spacing in
/// the airband); 10 kHz covers broadcast shortwave without pulling in the
/// neighbours on crowded bands.
pub const AM_BANDWIDTH_HZ: f32 = 10_000.0;

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
/// the window. Zoomed in, the window is not cut from the full-span FFT: it is
/// mixed to baseband, decimated to a little more than its own width and
/// transformed at that rate (see [`ZoomPath`]), so the bin width shrinks with
/// the zoom instead of staying the full span's.
pub const ZOOM_MAX: f64 = 32.0;
const ZOOM_TARGET_BINS: usize = 1024;

/// FFT size for the full-span spectrum on a display `bins` wide. A power of
/// two, 256..=8192. This spectrum draws the 1x view and feeds the scanner's
/// whole-span search; the zoomed view has its own transform.
fn fft_size_for(bins: usize) -> usize {
    bins.clamp(256, 8192).next_power_of_two().min(8192)
}

/// The decimated rate stays at least this many times the shown width, so the
/// window sits inside the last stage's flat passband — never on its skirt,
/// and never where something just outside the window folds back onto it.
const ZOOM_GUARD: f64 = 1.5;

/// How a zoomed window is produced from the sampled span.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ZoomPlan {
    /// Sample rate going in.
    fs_in: f64,
    /// Decimate-by-two stages, so the rate out is `fs_in / 2^stages`.
    stages: u32,
    /// Sample rate of the decimated stream, which the zoom FFT spans.
    rate_hz: f64,
    /// Size of that FFT.
    fft_size: usize,
}

impl ZoomPlan {
    /// Width of one bin of the zoom FFT.
    #[cfg(test)]
    fn bin_hz(&self) -> f64 {
        self.rate_hz / self.fft_size as f64
    }
}

/// Plan the zoom path for a window `shown_rate` wide on a display
/// `display_bins` wide: halve the rate while a halving still leaves the
/// guard band, then size the FFT so the window covers at least
/// `display_bins` bins (and under twice that). The bin width is then between
/// a half and one display pixel wide at any zoom, instead of the full span's.
///
/// With no stage left to take (the window is wider than a third of the
/// span) the transform runs at the full rate, so it is capped at 8192 as the
/// full-span one is; decimated, it is cheap enough to go to 16384.
fn zoom_plan(fs_in: f64, shown_rate: f64, display_bins: usize) -> ZoomPlan {
    let shown_rate = shown_rate.max(1.0);
    let mut rate_hz = fs_in;
    let mut stages = 0u32;
    while rate_hz / 2.0 >= shown_rate * ZOOM_GUARD && stages < 16 {
        rate_hz /= 2.0;
        stages += 1;
    }
    let want = (display_bins as f64 * rate_hz / shown_rate).ceil().max(256.0) as usize;
    let cap = if stages == 0 { 8192 } else { 16384 };
    let fft_size = want.next_power_of_two().min(cap);
    ZoomPlan { fs_in, stages, rate_hz, fft_size }
}

/// The window the display shows, in the terms the rest of the loop uses.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ShownWindow {
    /// Where the hardware LO sits relative to the receive frequency.
    lo_off: f64,
    /// The span available to show at 1x.
    full_rate: f64,
    /// The width shown at the current zoom.
    shown_rate: f64,
    /// The window's center, relative to the receive frequency.
    centre_off: f64,
    /// The window's center, absolute.
    shown_centre: f64,
}

impl ShownWindow {
    /// The FFT covers the sampled span around the *hardware* center. With
    /// the LO parked off-center that is not the window being shown, so the
    /// shown part is the bins either side of the requested frequency that
    /// stay inside the tuner's passband. Zoomed in, the window is on
    /// `view_hz`: what is being received, not the middle of the span (the
    /// operator is looking at a signal, not at a coordinate), or with span
    /// lock wherever the window is being held.
    fn new(rate_hz: f64, lo_on: bool, mode: SdrMode, zoom: f64, tuned_freq: f64, view_hz: f64) -> Self {
        let lo_off = lo_offset_for(rate_hz, lo_on, mode);
        let full_rate = display_rate_for(rate_hz, lo_on, mode);
        let shown_rate = full_rate / zoom;
        let want_center = if zoom > 1.0 { view_hz } else { tuned_freq };
        // Keep the window inside the part of the span that is usable.
        let limit = (full_rate / 2.0 - shown_rate / 2.0).max(0.0);
        let centre_off = (want_center - tuned_freq).clamp(-limit, limit);
        Self { lo_off, full_rate, shown_rate, centre_off, shown_centre: tuned_freq + centre_off }
    }

    /// The window's center as an offset from the hardware center — what the
    /// zoom path mixes down to DC.
    fn mix_offset_hz(&self) -> f64 {
        self.centre_off - self.lo_off
    }

    /// Whether a channel `bandwidth_hz` wide at `hz` sits inside the window
    /// (at least half of the window's own width when the channel is wider).
    fn holds(&self, hz: f64, bandwidth_hz: f64) -> bool {
        let margin = (bandwidth_hz / 2.0).min(self.shown_rate / 4.0);
        (hz - self.shown_centre).abs() <= self.shown_rate / 2.0 - margin
    }
}

/// Where the zoomed window goes for this block. Following, it is on the
/// receiver. Held (span lock), it stays where it is until a retune puts the
/// receiver outside it, and then it moves onto the receiver; a window panned
/// away from a receiver that has not moved stays panned.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ZoomView {
    hold: bool,
    /// The held window's center, absolute.
    center_hz: f64,
    /// The receive frequency the hold last saw, so a retune is told from a
    /// receiver that has not moved.
    seen_inspect_hz: f64,
}

impl ZoomView {
    fn new(inspect_hz: f64) -> Self {
        Self { hold: false, center_hz: inspect_hz, seen_inspect_hz: inspect_hz }
    }

    fn window(&mut self, rate_hz: f64, lo_on: bool, mode: SdrMode, zoom: f64, tuned_freq: f64, inspect_hz: f64) -> ShownWindow {
        if !self.hold || zoom <= 1.0 {
            self.seen_inspect_hz = inspect_hz;
            return ShownWindow::new(rate_hz, lo_on, mode, zoom, tuned_freq, inspect_hz);
        }
        let held = ShownWindow::new(rate_hz, lo_on, mode, zoom, tuned_freq, self.center_hz);
        let retuned = (inspect_hz - self.seen_inspect_hz).abs() >= 1.0;
        self.seen_inspect_hz = inspect_hz;
        if retuned && !held.holds(inspect_hz, f64::from(mode.bandwidth_hz())) {
            self.center_hz = inspect_hz;
            return ShownWindow::new(rate_hz, lo_on, mode, zoom, tuned_freq, inspect_hz);
        }
        held
    }
}

/// The bins of a spectrum `n_src` bins wide at `src_rate` that cover a
/// window `shown_rate` wide centered `src_off` Hz from the spectrum's DC.
/// Whole bins, so the window's true edges land up to half a bin inside
/// them: the frame's axis is stamped from the bins (see [`frame_axis`]),
/// never from the request.
fn window_bins(src_rate: f64, n_src: usize, src_off: f64, shown_rate: f64) -> (usize, usize) {
    let bin_of = |offset_hz: f64| -> usize {
        let f = (offset_hz / src_rate + 0.5) * n_src as f64;
        f.round().clamp(0.0, n_src as f64 - 1.0) as usize
    };
    let lo = bin_of(src_off - shown_rate / 2.0);
    let hi = bin_of(src_off + shown_rate / 2.0).max(lo + 1).min(n_src);
    (lo, hi)
}

/// The frequency axis a frame carries: `center_hz` and `rate_hz` such that
/// bin 0 is at `center - rate / 2` and the last bin at `center + rate / 2`,
/// which is where every viewer draws them (and where `channel_level`,
/// `notch_display` and `find_peaks` read them). For the bins `lo..hi` cut
/// from an fftshifted spectrum of `n_src` bins spanning `src_rate` with its
/// DC at `src_dc_hz`, bin `k` sits at `(k - n_src / 2)` bin widths from DC,
/// so the rate is `n - 1` bin widths and the center the midpoint of the
/// first and last bins. Stamping the requested window instead drew every
/// signal up to a bin off: the bins are whole, the request was not.
#[derive(Clone, Copy, Debug, PartialEq)]
struct FrameAxis {
    center_hz: f64,
    rate_hz: f64,
}

fn frame_axis(src_dc_hz: f64, src_rate: f64, n_src: usize, lo: usize, hi: usize) -> FrameAxis {
    let bin_hz = src_rate / n_src.max(1) as f64;
    let n = hi.saturating_sub(lo).max(1);
    let first = src_dc_hz + (lo as f64 - (n_src / 2) as f64) * bin_hz;
    let span = (n - 1) as f64 * bin_hz;
    FrameAxis { center_hz: first + span / 2.0, rate_hz: span.max(bin_hz) }
}

impl FrameAxis {
    /// Where bin `i` of an `n`-bin frame on this axis sits.
    #[cfg(test)]
    fn bin_hz(&self, i: usize, n: usize) -> f64 {
        self.center_hz - self.rate_hz / 2.0 + i as f64 * self.rate_hz / (n.max(2) - 1) as f64
    }
}

/// The size of the transform the display is drawn from, for the status:
/// the zoom path's when zoomed in, else the full span's.
fn displayed_fft_size(full: usize, zoom: f64, rate_hz: f64, lo_on: bool, mode: SdrMode, display_bins: usize) -> usize {
    if zoom > 1.0 {
        zoom_plan(rate_hz, display_rate_for(rate_hz, lo_on, mode) / zoom, display_bins).fft_size
    } else {
        full
    }
}

/// The zoomed window's own spectrum: the span mixed so the window's center
/// is at DC, decimated by halfband stages to a rate just over the window's
/// width, and Welch-transformed there. At 8 MS/s zoomed 32x onto a 2048-bin
/// display that is 250 kHz across 2048 bins of 122 Hz, where one FFT over
/// the span, capped at 8192 bins, gave 256 bins of 977 Hz and drew a 3 kHz
/// sideband signal three bins wide.
///
/// The full-span spectrum keeps running beside it (at a lower duty), since
/// the scanner searches the whole span whatever the display shows.
struct ZoomPath {
    plan: ZoomPlan,
    offset_hz: f64,
    nco: scannerd_dsp::Nco,
    decim: scannerd_dsp::HalfbandChain,
    spectrum: Spectrum,
    mixed: Vec<num_complex::Complex32>,
    iq: Vec<num_complex::Complex32>,
    /// This interval's Welch mean, its per-bin peak, and the mean smoothed
    /// across bins for the trace — the zoom's counterparts of `pwr`,
    /// `pwr_peak` and `smoothed`.
    pwr: Vec<f32>,
    pwr_peak: Vec<f32>,
    smoothed: Vec<f32>,
    /// At least one segment has been taken since the path was built or
    /// retuned, so `pwr` describes the window and not a placeholder.
    ready: bool,
}

impl ZoomPath {
    fn new(plan: ZoomPlan, shown_rate: f64, offset_hz: f64) -> Self {
        let mut nco = scannerd_dsp::Nco::new();
        nco.set_freq(offset_hz, plan.fs_in);
        Self {
            plan,
            offset_hz,
            nco,
            decim: scannerd_dsp::HalfbandChain::new(plan.fs_in, shown_rate / 2.0, plan.stages),
            spectrum: Spectrum::new(plan.fft_size),
            mixed: Vec::new(),
            iq: Vec::new(),
            pwr: Vec::with_capacity(plan.fft_size),
            pwr_peak: Vec::with_capacity(plan.fft_size),
            smoothed: Vec::with_capacity(plan.fft_size),
            ready: false,
        }
    }

    /// Move the window: the mixer follows at once, and the partial segment
    /// and accumulators from the old window are dropped rather than
    /// averaged into the first frame of the new one. The filters' few
    /// samples of history are left; their transient is microseconds.
    fn retune(&mut self, offset_hz: f64) {
        if (offset_hz - self.offset_hz).abs() < 0.01 {
            return;
        }
        self.offset_hz = offset_hz;
        self.nco.set_freq(offset_hz, self.plan.fs_in);
        self.spectrum.clear();
        self.ready = false;
    }

    /// A break in the sample timeline: nothing from before it is stitched
    /// onto what follows.
    fn reset(&mut self) {
        self.nco.reset_phase();
        self.decim.reset();
        self.spectrum.clear();
        self.ready = false;
    }

    fn ingest(&mut self, span_iq: &[num_complex::Complex32]) {
        self.nco.mix(span_iq, &mut self.mixed);
        self.decim.process(&self.mixed, &mut self.iq);
        self.spectrum.ingest_welch(&self.iq);
    }

    /// Take this frame's estimate. Returns the segments it covers; with
    /// none, the previous frame's arrays are left in place.
    fn take(&mut self) -> usize {
        let full_duty =
            (self.plan.rate_hz / f64::from(DEFAULT_FPS) / (self.plan.fft_size as f64 / 2.0)).ceil() as usize;
        self.spectrum.set_max_segments_per_take((full_duty * 13 / 10).max(8));
        let nseg = self.spectrum.take_dbfs(&mut self.pwr, Some(&mut self.pwr_peak));
        if nseg > 0 {
            self.ready = true;
            smooth_bins(&self.pwr, 3, &mut self.smoothed);
        }
        nseg
    }
}

/// What the inspect chain is being used for. This picks the channel width, the
/// demodulator, and what gets fed the resulting audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SdrMode {
    /// Narrowband FM plus the full digital classifier. The default.
    #[default]
    Nfm,
    /// Amplitude modulation: HF airband and broadcast. The classifier's FM
    /// discriminators have nothing to say about an envelope-modulated carrier.
    Am,
    /// Single sideband voice, upper or lower: HF amateur and utility
    /// traffic. The suppressed carrier is the tuned frequency; the audio is
    /// the 300-2800 Hz above it (USB) or below it (LSB).
    Usb,
    Lsb,
    /// Broadcast FM, for listening. The classifier is not run: none of what it
    /// looks for exists in a 200 kHz music channel.
    Wfm,
    /// Narrowband FM run through every packet and paging decoder at once —
    /// AX.25 at 1200 baud, POCSAG at 512/1200/2400, and FLEX — so whatever is
    /// in the passband is decoded without having to be told which it is.
    Packet,
    /// P25 Phase 1, or explicitly configured Phase 2, decoded to voice.
    P25,
    /// DMR Tier II, decoded to voice through the AMBE vocoder.
    Dmr,
    /// NXDN48/NXDN96 conventional voice, decoded through the AMBE+2 vocoder.
    /// Both symbol rates are acquired concurrently; whichever locks wins.
    Nxdn,
    /// Everything at once: the classifier, both digital voice receivers, and
    /// every packet and paging decoder, all on the same channel. Costs more
    /// CPU than picking one, and answers "what is this?" without being told.
    Auto,
    /// Whole-band paging sweep: one POCSAG+FLEX bank walked across the 25 kHz
    /// channels of the paging band around the tuned frequency. The classifier
    /// and voice receivers do not run — a pager channel has no voice in it.
    Pager,
    /// Voice scan: a peak-driven sweep across a configured range that locks
    /// onto whatever decodes to voice and holds for the call. The state
    /// machine lives in [`crate::scan`]; the receivers it points at the
    /// candidate frequencies live in the parallel slot pool
    /// ([`ScanSlotRig`]), not here — there is one pool, not one per rebuild
    /// of this demodulator.
    Scan,
    /// Dedicated Motorola FLEX pager decoding on the tuned channel with
    /// 4-FSK/2-FSK symbol eye and discriminator level scope.
    Flex,
    /// Marine AIS: 9600 baud GMSK on both VHF channels, 161.975 and
    /// 162.025 MHz, decoded at once from the span wherever they sit in it.
    Ais,
    /// HF weak-signal data: the upper-sideband demodulator over a fixed
    /// 100–3100 Hz, its audio cut into UTC slots and decoded at the end of
    /// each, 15 s for FT8 and 7.5 s for FT4. The tuned frequency is the
    /// dial (the suppressed carrier), as the bands publish it.
    Ft8,
    Ft4,
    /// HF radiofax: weather charts and pictures on a 1900 Hz FM subcarrier,
    /// the dial 1.9 kHz below the station's assigned frequency.
    Wefax,
    /// HF text on upper sideband, each decoder finding its signal in the
    /// passband itself: RTTY (Baudot FSK), NAVTEX (SITOR-B, 518 and 490
    /// kHz), and PSK31, which copies every signal in the passband at once.
    Rtty,
    Navtex,
    Psk31,
    /// HF utility and data links on upper sideband, each decoder finding
    /// its signal in the passband: SITOR A and B, MF/HF DSC, HFDL, PACTOR-I
    /// and the NATO serial-tone modems.
    Sitor,
    Dsc,
    Hfdl,
    Pactor,
    Stanag,
    /// 2G ALE (MIL-STD-188-141): the same serial-tone decoder, its ALE
    /// calls and soundings shown.
    Ale,
    /// Amateur Morse (every signal in the passband), JS8Call (FT8's
    /// waveform in free text) and slow-scan television.
    Cw,
    Js8,
    Sstv,
    /// WSPR: two-minute beacon slots decoded for call, locator and power.
    Wspr,
    /// Olivia (its common settings tried at once), MFSK16/32/64 with
    /// pictures, and Feld Hell, read as a picture.
    Olivia,
    Mfsk,
    Hell,
    /// WWV and WWVH on AM, their 100 Hz time code decoded from the
    /// envelope.
    Wwv,
    /// Digital Radio Mondiale: the OFDM multiplex decoded from the channel
    /// I/Q, the audio its AAC frames decoded.
    Drm,
}

/// The audio the FT8/FT4 decoder searches, above the dial: the whole of a
/// sideband transmitter's passband, wider than voice so the signals near
/// either edge of the sub-band are heard.
const FT8_LOW_HZ: f32 = 100.0;
const FT8_HIGH_HZ: f32 = 3_100.0;
/// Radiofax's audio: 1500 Hz black to 2300 Hz white, and 400 Hz either side
/// for the picture's detail and a station a little off frequency.
const WEFAX_LOW_HZ: f32 = 1_000.0;
const WEFAX_HIGH_HZ: f32 = 2_800.0;
/// Morse's audio: where the CW skimmer looks by default, the usual pitches
/// and some either side.
const CW_LOW_HZ: f32 = 200.0;
const CW_HIGH_HZ: f32 = 1_200.0;
/// DRM's widest channel (spectrum occupancy 5).
const DRM_BANDWIDTH_HZ: f32 = 20_000.0;

/// The operator's filter for each mode that has one, as (low, high) edges
/// in Hz relative to the carrier. One radio per server, so one table.
static FILTERS: std::sync::Mutex<Option<std::collections::HashMap<SdrMode, (f32, f32)>>> =
    std::sync::Mutex::new(None);

fn filter_override(mode: SdrMode) -> Option<(f32, f32)> {
    FILTERS.lock().ok()?.as_ref()?.get(&mode).copied()
}

/// The operator's filter shape for each sideband mode that has one; the
/// rest are [`FilterShape::Normal`]. Kept apart from the edges, so going
/// back to the mode's own width keeps the shape chosen.
static SHAPES: std::sync::Mutex<Option<std::collections::HashMap<SdrMode, FilterShape>>> = std::sync::Mutex::new(None);

/// The widest a sideband filter may be, and the furthest its edge may sit
/// from the carrier: room for extended SSB's 50–4500 Hz and for every FT8
/// station above the dial, under the decoder's 6 kHz Nyquist.
pub const SIDEBAND_MAX_HZ: f32 = 5_000.0;

/// Which filters can be set, and how far. Sideband takes both edges (width
/// and shift); AM, NFM and WFM a symmetric width. The digital modes keep the
/// filters their decoders were built for.
#[derive(Clone, Copy, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FilterRange {
    pub min_width_hz: f32,
    pub max_width_hz: f32,
    /// Furthest an edge may sit from the carrier.
    pub max_edge_hz: f32,
    pub symmetric: bool,
    /// Whether the skirt's shape can be chosen (sharp, normal, soft).
    pub shaped: bool,
}

impl SdrMode {
    /// Demodulated as single sideband, so there is no carrier to measure.
    pub fn is_sideband(self) -> bool {
        matches!(self, SdrMode::Usb | SdrMode::Lsb | SdrMode::Ft8 | SdrMode::Ft4 | SdrMode::Wefax | SdrMode::Rtty | SdrMode::Navtex | SdrMode::Psk31 | SdrMode::Sitor | SdrMode::Dsc | SdrMode::Hfdl | SdrMode::Pactor | SdrMode::Stanag | SdrMode::Ale | SdrMode::Cw | SdrMode::Js8 | SdrMode::Sstv | SdrMode::Wspr | SdrMode::Olivia | SdrMode::Mfsk | SdrMode::Hell)
    }

    /// The slot decoder a data mode runs on its sideband audio.
    fn ft8_protocol(self) -> Option<scannerd_engine::ft8::Protocol> {
        match self {
            SdrMode::Ft8 => Some(scannerd_engine::ft8::Protocol::Ft8),
            SdrMode::Ft4 => Some(scannerd_engine::ft8::Protocol::Ft4),
            _ => None,
        }
    }

    pub fn filter_range(self) -> Option<FilterRange> {
        let sym = |lo: f32, hi: f32| FilterRange { min_width_hz: lo, max_width_hz: hi, max_edge_hz: hi / 2.0, symmetric: true, shaped: false };
        let side = |min: f32| FilterRange { min_width_hz: min, max_width_hz: SIDEBAND_MAX_HZ, max_edge_hz: SIDEBAND_MAX_HZ, symmetric: false, shaped: true };
        match self {
            SdrMode::Usb | SdrMode::Lsb => Some(side(200.0)),
            // The slot decoder searches whatever the filter passes; narrower
            // than 500 Hz holds too few stations to be worth a slot.
            SdrMode::Ft8 | SdrMode::Ft4 | SdrMode::Js8 => Some(side(500.0)),
            // Morse is listened to as well as skimmed: as narrow as a CW
            // filter goes.
            SdrMode::Cw => Some(side(100.0)),
            SdrMode::Am | SdrMode::Wwv => Some(sym(2_000.0, 20_000.0)),
            SdrMode::Nfm => Some(sym(2_500.0, INSPECT_BANDWIDTH_HZ)),
            SdrMode::Wfm => Some(sym(50_000.0, 230_000.0)),
            _ => None,
        }
    }

    /// Check and tidy a requested filter for this mode: symmetric modes
    /// are centered on the carrier, sideband edges must sit on its side.
    pub fn normalize_filter(self, low: f32, high: f32) -> std::result::Result<(f32, f32), String> {
        let Some(r) = self.filter_range() else {
            return Err(format!("{}'s filter is fixed to what its decoder needs", self.capabilities().label));
        };
        if !(low.is_finite() && high.is_finite()) || high <= low {
            return Err("the high edge must be above the low edge".into());
        }
        let (lo, hi) = if r.symmetric {
            let w = (high - low).clamp(r.min_width_hz, r.max_width_hz);
            (-w / 2.0, w / 2.0)
        } else {
            let (lo, hi) = (low.clamp(-r.max_edge_hz, r.max_edge_hz), high.clamp(-r.max_edge_hz, r.max_edge_hz));
            match self {
                SdrMode::Usb | SdrMode::Ft8 | SdrMode::Ft4 | SdrMode::Cw | SdrMode::Js8 if lo < 0.0 => return Err("upper sideband edges sit above the carrier".into()),
                SdrMode::Lsb if hi > 0.0 => return Err("lower sideband edges sit below the carrier".into()),
                _ => {}
            }
            if hi - lo < r.min_width_hz {
                return Err(format!("narrowest is {} Hz", r.min_width_hz));
            }
            (lo, hi.min(lo + r.max_width_hz))
        };
        Ok((lo, hi))
    }

    /// Set (or with `None` clear) this mode's filter, server-wide.
    pub fn set_filter(self, f: Option<(f32, f32)>) {
        if let Ok(mut g) = FILTERS.lock() {
            let map = g.get_or_insert_with(Default::default);
            match f {
                Some(f) => map.insert(self, f),
                None => map.remove(&self),
            };
        }
    }

    /// Set (or with `None` go back to normal) this mode's filter shape.
    pub fn set_filter_shape(self, shape: Option<FilterShape>) {
        if let Ok(mut g) = SHAPES.lock() {
            let map = g.get_or_insert_with(Default::default);
            match shape.filter(|s| *s != FilterShape::Normal) {
                Some(s) => map.insert(self, s),
                None => map.remove(&self),
            };
        }
    }

    /// The shape of this mode's passband skirt: the operator's where the
    /// mode takes one, otherwise normal.
    pub fn filter_shape(self) -> FilterShape {
        if !self.filter_range().is_some_and(|r| r.shaped) {
            return FilterShape::Normal;
        }
        SHAPES.lock().ok().and_then(|g| g.as_ref()?.get(&self).copied()).unwrap_or_default()
    }

    /// Whether the operator has set this mode's edges.
    pub fn filter_custom(self) -> bool {
        filter_override(self).is_some()
    }

    /// The passband edges in use, relative to the carrier.
    pub fn filter_edges(self) -> (f32, f32) {
        filter_override(self).unwrap_or_else(|| {
            let c = self.default_passband_offset_hz();
            let w = self.default_bandwidth_hz();
            (c - w / 2.0, c + w / 2.0)
        })
    }

    /// The passband width in use: the operator's filter, or the mode's own.
    fn bandwidth_hz(self) -> f32 {
        let (lo, hi) = self.filter_edges();
        hi - lo
    }

    /// The mode's own passband width, which the LO plan is laid out around
    /// so that moving the filter never moves the tuner.
    fn default_bandwidth_hz(self) -> f32 {
        match self {
            SdrMode::Nfm => INSPECT_BANDWIDTH_HZ,
            SdrMode::Am => AM_BANDWIDTH_HZ,
            // The voice passband itself, drawn offset from the carrier
            // (see passband_offset_hz).
            SdrMode::Usb | SdrMode::Lsb => SSB_HIGH_HZ - SSB_LOW_HZ,
            SdrMode::Ft8 | SdrMode::Ft4 => FT8_HIGH_HZ - FT8_LOW_HZ,
            // Black at 1500 Hz and white at 2300, with the picture's own
            // sidebands either side.
            SdrMode::Wefax => WEFAX_HIGH_HZ - WEFAX_LOW_HZ,
            // Morse: the skimmer's band, 200–1200 Hz above the dial.
            SdrMode::Cw => CW_HIGH_HZ - CW_LOW_HZ,
            SdrMode::Rtty | SdrMode::Navtex | SdrMode::Psk31 | SdrMode::Sitor | SdrMode::Dsc | SdrMode::Hfdl | SdrMode::Pactor | SdrMode::Stanag | SdrMode::Ale | SdrMode::Js8 | SdrMode::Sstv | SdrMode::Wspr | SdrMode::Olivia | SdrMode::Mfsk | SdrMode::Hell => FT8_HIGH_HZ - FT8_LOW_HZ,
            SdrMode::Wwv => AM_BANDWIDTH_HZ,
            // The widest DRM channel, 20 kHz (spectrum occupancy 5): the
            // receiver finds which carriers are its own.
            SdrMode::Drm => DRM_BANDWIDTH_HZ,
            // What the mode actually DECODES: the walking bank covers this
            // whole span, so the display should shade it — not the 15 kHz of
            // the (idle) per-channel demod chain.
            SdrMode::Pager => (2.0 * PAGER_HALF_BAND_HZ) as f32,
            // Both channels and their filters: 161.96 to 162.04 MHz.
            SdrMode::Ais => AIS_BANDWIDTH_HZ,
            SdrMode::Wfm => WFM_BANDWIDTH_HZ,
            SdrMode::Packet | SdrMode::Flex => PACKET_BANDWIDTH_HZ,
            SdrMode::P25 => C4FM_BANDWIDTH_HZ,
            SdrMode::Dmr => DMR_BANDWIDTH_HZ,
            SdrMode::Nxdn => NXDN_BANDWIDTH_HZ,
            // Wide enough for every narrowband candidate at once.
            SdrMode::Auto => INSPECT_BANDWIDTH_HZ,
            // The scan tests AM, NFM, P25 and DMR candidates through one
            // chain; 25 kHz passes all four channel widths.
            SdrMode::Scan => INSPECT_BANDWIDTH_HZ,
        }
    }


    /// What the scope trace represents, so the browser knows which instrument
    /// to draw. A 0–4 kHz audio spectrum says nothing useful about C4FM, and an
    /// eye diagram says nothing useful about a broadcast station.
    fn scope_kind(self) -> &'static str {
        match self {
            // Auto is watching for digital, so the eye is the useful view.
            SdrMode::P25 | SdrMode::Dmr | SdrMode::Nxdn | SdrMode::Auto | SdrMode::Flex => {
                "symbols"
            }
            SdrMode::Wfm => "mpx",
            _ => "audio",
        }
    }

    /// Symbol rate for the modes whose scope is drawn against one.
    fn symbol_rate_hz(self) -> f32 {
        match self {
            SdrMode::P25 | SdrMode::Dmr | SdrMode::Auto => 4_800.0,
            // NXDN runs both rates concurrently; the eye is drawn against the
            // narrow conventional rate (NXDN48) as the common reference.
            SdrMode::Nxdn => 2_400.0,
            SdrMode::Packet => 1_200.0,
            SdrMode::Flex => 1_600.0,
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
            SdrMode::P25 | SdrMode::Dmr | SdrMode::Nxdn | SdrMode::Auto | SdrMode::Flex => 48_000.0,
            // The live pager channel, box-decimated from the bank's channel
            // rate down to 48 kS/s: a ±24 kHz axis brackets the ±4.8 kHz
            // FLEX deviation levels with room for several kHz of drift, and
            // the longer window shows whole symbol runs instead of a 5 ms
            // slice. (The channel itself runs at span/round(span/96 kHz),
            // which is why this is a decimation and not a passthrough.)
            SdrMode::Pager => 48_000.0,
            // AM voice/broadcast channel: 12 kS/s delivers a 6 kHz Nyquist
            // audio spectrum that matches the AM IF filter passband (6.0 kHz).
            SdrMode::Am | SdrMode::Wwv => 12_000.0,
            // Sideband voice and FT8 may be 5 kHz wide.
            SdrMode::Usb | SdrMode::Lsb | SdrMode::Ft8 | SdrMode::Ft4 => 12_000.0,
            _ => 8_000.0,
        }
    }

    /// Where the passband's center sits relative to the tuned frequency:
    /// zero for every mode except sideband, whose audio is all on one side
    /// of the suppressed carrier.
    pub fn passband_offset_hz(self) -> f32 {
        let (lo, hi) = self.filter_edges();
        (lo + hi) / 2.0
    }

    fn default_passband_offset_hz(self) -> f32 {
        let center = (SSB_LOW_HZ + SSB_HIGH_HZ) / 2.0;
        match self {
            SdrMode::Usb => center,
            SdrMode::Lsb => -center,
            SdrMode::Cw => (CW_LOW_HZ + CW_HIGH_HZ) / 2.0,
            SdrMode::Ft8 | SdrMode::Ft4 | SdrMode::Rtty | SdrMode::Navtex | SdrMode::Psk31 | SdrMode::Sitor | SdrMode::Dsc | SdrMode::Hfdl | SdrMode::Pactor | SdrMode::Stanag | SdrMode::Ale | SdrMode::Js8 | SdrMode::Sstv | SdrMode::Wspr | SdrMode::Olivia | SdrMode::Mfsk | SdrMode::Hell => (FT8_LOW_HZ + FT8_HIGH_HZ) / 2.0,
            SdrMode::Wefax => (WEFAX_LOW_HZ + WEFAX_HIGH_HZ) / 2.0,
            _ => 0.0,
        }
    }

    /// Width of the inspect chain's channel filter. Sideband needs both sides
    /// of the carrier into the demodulator, which picks one; everything
    /// else filters to the width it displays.
    pub fn chain_bandwidth(self) -> f32 {
        self.chain_bandwidth_hz()
    }

    fn chain_bandwidth_hz(self) -> f32 {
        match self {
            SdrMode::Usb | SdrMode::Lsb | SdrMode::Ft8 | SdrMode::Ft4 | SdrMode::Wefax | SdrMode::Rtty | SdrMode::Navtex | SdrMode::Psk31 | SdrMode::Sitor | SdrMode::Dsc | SdrMode::Hfdl | SdrMode::Pactor | SdrMode::Stanag | SdrMode::Ale | SdrMode::Cw | SdrMode::Js8 | SdrMode::Sstv | SdrMode::Wspr | SdrMode::Olivia | SdrMode::Mfsk | SdrMode::Hell => {
                // Both sides of the carrier out to the far edge, and the
                // widest skirt past it, so the channel filter never shapes
                // the band before the demodulator's own filter does.
                let (lo, hi) = self.filter_edges();
                2.0 * (lo.abs().max(hi.abs()) + FilterShape::Soft.skirt_hz() as f32)
            }
            _ => self.bandwidth_hz(),
        }
    }

    fn target_rate(self) -> f64 {
        match self {
            SdrMode::Wfm => WFM_IF_RATE,
            SdrMode::Packet | SdrMode::Flex => PACKET_INSPECT_RATE,
            _ => INSPECT_RATE,
        }
    }

    /// Authoritative per-mode capability description. The UI has historically
    /// hardcoded what each mode can do and drifted from the truth (a mode
    /// count in the README, an NXDN button that did not exist, scope labels
    /// that did not match the binary frame). This is the single source the
    /// browser and any other client should read: what the mode is called,
    /// what it delivers (audio and/or decoded data), and how its scope is
    /// drawn.
    pub fn capabilities(self) -> ModeCapabilities {
        let (label, kind, description) = match self {
            SdrMode::Nfm => (
                "NFM",
                "voice",
                "Narrowband FM plus the full digital classifier (CTCSS/DCS/MDC/POCSAG/FLEX/APRS detection)",
            ),
            SdrMode::Am => ("AM", "voice", "Amplitude modulation: airband and broadcast"),
            SdrMode::Usb => ("USB", "voice", "Upper sideband voice: HF above 10 MHz, VHF/UHF weak-signal"),
            SdrMode::Lsb => ("LSB", "voice", "Lower sideband voice: HF below 10 MHz"),
            SdrMode::Wfm => ("WFM", "voice", "Broadcast FM; classifier off"),
            SdrMode::Packet => (
                "PACKET",
                "data",
                "Every packet and paging decoder on the tuned channel: AX.25 1200, POCSAG 512/1200/2400, FLEX",
            ),
            SdrMode::P25 => ("P25", "voice+data", "P25 Phase 1 voice and configured Phase 2 channels; Phase 1 trunk grants followed"),
            SdrMode::Dmr => ("DMR", "voice+data", "DMR Tier II voice through the AMBE vocoder"),
            SdrMode::Nxdn => ("NXDN", "voice+data", "NXDN48/NXDN96 conventional voice through AMBE+2; Type-C/D call assignments surfaced"),
            SdrMode::Auto => ("AUTO", "voice+data", "Every decoder at once on the tuned channel: classifier, P25, DMR, and all packet/paging paths"),
            SdrMode::Pager => ("PAGER", "data", "POCSAG+FLEX bank walked across the 25 kHz paging channels around the tuned frequency"),
            SdrMode::Flex => ("FLEX", "data", "Dedicated Motorola FLEX demodulator with symbol eye and discriminator scope"),
            SdrMode::Ais => ("AIS", "data", "Marine AIS: positions, names and voyages of ships, base stations and aids to navigation, both channels (161.975 and 162.025 MHz) at once"),
            SdrMode::Ft8 => ("FT8", "data", "FT8 on upper sideband: every signal in the filter's passband above the dial (100–3100 Hz unless widened, up to 5 kHz), decoded at the end of every 15 s UTC slot"),
            SdrMode::Ft4 => ("FT4", "data", "FT4 on upper sideband: every signal in the filter's passband above the dial (100–3100 Hz unless widened, up to 5 kHz), decoded at the end of every 7.5 s UTC slot"),
            SdrMode::Wefax => ("WEFAX", "data", "HF radiofax on upper sideband: the 1900 Hz subcarrier decoded row by row into a picture, started by the station's tones"),
            SdrMode::Rtty => ("RTTY", "data", "Baudot RTTY on upper sideband: the tone pair found in 100–3100 Hz, 45.45 baud 170 Hz by default"),
            SdrMode::Navtex => ("NAVTEX", "data", "NAVTEX maritime safety messages (SITOR-B, 100 baud): tune 1 kHz below 518 or 490 kHz"),
            SdrMode::Psk31 => ("PSK31", "data", "BPSK31 on upper sideband: every signal in 200–3000 Hz copied at once"),
            SdrMode::Sitor => ("SITOR", "data", "SITOR on upper sideband: mode A (ARQ) links and mode B (FEC) broadcasts, 100 baud, 170 Hz shift, told apart by their timing"),
            SdrMode::Dsc => ("DSC", "data", "MF/HF digital selective calling (ITU-R M.493): distress, safety and routine calls; tune 1.7 kHz below 2187.5, 4207.5, 6312, 8414.5, 12577 or 16804.5 kHz"),
            SdrMode::Hfdl => ("HFDL", "data", "HF data link (ARINC 635): ground station squitters and aircraft ACARS, 1800 baud PSK at 1440 Hz above the dial"),
            SdrMode::Pactor => ("PACTOR", "data", "PACTOR-I links and broadcasts, 100 and 200 baud FSK; PACTOR-II to IV are proprietary and not decoded"),
            SdrMode::Stanag => ("STANAG", "data", "NATO HF serial-tone modems: STANAG 4285 and MIL-STD-188-110 identified and demodulated, STANAG 5066 headers read when the data is clear"),
            SdrMode::Ale => ("ALE", "data", "2G ALE (MIL-STD-188-141): 8-FSK at 125 baud, soundings and calls with their callsigns; STANAG serial tones heard alongside are named too"),
            SdrMode::Cw => ("CW", "data", "Morse on upper sideband: every signal in the filter's passband decoded at once, each at its own pitch and speed (8–50 WPM)"),
            SdrMode::Js8 => ("JS8", "data", "JS8Call on upper sideband: free text, heartbeats and directed messages, at all four speeds (normal, fast, turbo, slow)"),
            SdrMode::Wspr => ("WSPR", "data", "WSPR on upper sideband: every beacon in 1400–1600 Hz above the dial, decoded at the end of each even-minute 2-minute slot, down to about −28 dB"),
            SdrMode::Olivia => ("OLIVIA", "data", "Olivia MFSK chat: the common settings (8/250, 8/500, 16/500, 32/1000, 16/1000, 4/250) tried at once around 1500 Hz, copying down to about −12 dB"),
            SdrMode::Mfsk => ("MFSK", "data", "MFSK16, 32 and 64 text and pictures, as Shortwave Radiogram sends them; the signal found anywhere in 300–2700 Hz"),
            SdrMode::Hell => ("HELL", "data", "Feld Hell: the carrier's on-off columns painted as a ribbon, read by eye"),
            SdrMode::Sstv => ("SSTV", "data", "Slow-scan television: Martin, Scottie, Robot, PD and Wraase pictures, found by their VIS code and drawn in color"),
            SdrMode::Wwv => ("WWV", "data", "WWV and WWVH on AM: the 100 Hz time code decoded to UTC, DUT1 and DST, and which station is heard"),
            SdrMode::Drm => ("DRM", "voice+data", "Digital Radio Mondiale (DRM30): service labels, text messages and AAC audio from the OFDM multiplex"),
            SdrMode::Scan => ("SCAN", "voice+data", "Peak-driven voice scan over a configured range: AM/NFM/P25/DMR slots, locks and holds what decodes"),
        };
        ModeCapabilities {
            mode: self,
            label,
            kind,
            description,
            // Every mode streams something audible: the data modes carry
            // their live channel (PAGER's walking-bank channel audio, FLEX's
            // gated monitor) so the operator can hear what is being decoded.
            delivers_audio: self != SdrMode::Ais,
            // WFM's are RDS: the station's name and RadioText.
            delivers_decode_events: true,
            bandwidth_hz: self.default_bandwidth_hz(),
            filter: self.filter_range(),
            scope_kind: self.scope_kind(),
            symbol_rate_hz: self.symbol_rate_hz(),
            passband_offset_hz: self.default_passband_offset_hz(),
            capture: match self {
                // The classifier chain: I/Q, its discriminator and the
                // monitor audio, all at the channel rate.
                SdrMode::Nfm | SdrMode::Packet | SdrMode::Flex | SdrMode::Auto => {
                    &["iq", "voice", "discriminator"]
                }
                // Audio at another rate (AM, the vocoders) or none (scan
                // between locks): the channel I/Q only.
                // Sideband audio comes out at the channel rate, so the ring
                // keeps it with the I/Q.
                SdrMode::Usb | SdrMode::Lsb | SdrMode::Ft8 | SdrMode::Ft4 | SdrMode::Wefax | SdrMode::Rtty | SdrMode::Navtex | SdrMode::Psk31 | SdrMode::Sitor | SdrMode::Dsc | SdrMode::Hfdl | SdrMode::Pactor | SdrMode::Stanag | SdrMode::Ale | SdrMode::Cw | SdrMode::Js8 | SdrMode::Sstv | SdrMode::Wspr | SdrMode::Olivia | SdrMode::Mfsk | SdrMode::Hell => &["iq", "voice"],
                SdrMode::Am | SdrMode::Wwv | SdrMode::Drm | SdrMode::P25 | SdrMode::Dmr | SdrMode::Nxdn | SdrMode::Scan => &["iq"],
                // WFM's I/Q is 256 kHz and unreplayable; PAGER walks a bank.
                SdrMode::Wfm | SdrMode::Pager | SdrMode::Ais => &[],
            },
        }
    }

    /// Every mode the server accepts, in UI order.
    pub fn all() -> &'static [SdrMode] {
        &[
            SdrMode::Nfm,
            SdrMode::Am,
            SdrMode::Usb,
            SdrMode::Lsb,
            SdrMode::Wfm,
            SdrMode::P25,
            SdrMode::Dmr,
            SdrMode::Nxdn,
            SdrMode::Packet,
            SdrMode::Pager,
            SdrMode::Flex,
            SdrMode::Ais,
            SdrMode::Ft8,
            SdrMode::Ft4,
            SdrMode::Wefax,
            SdrMode::Rtty,
            SdrMode::Navtex,
            SdrMode::Psk31,
            SdrMode::Sitor,
            SdrMode::Dsc,
            SdrMode::Hfdl,
            SdrMode::Pactor,
            SdrMode::Stanag,
            SdrMode::Ale,
            SdrMode::Cw,
            SdrMode::Js8,
            SdrMode::Sstv,
            SdrMode::Wspr,
            SdrMode::Olivia,
            SdrMode::Mfsk,
            SdrMode::Hell,
            SdrMode::Wwv,
            SdrMode::Drm,
            SdrMode::Auto,
            SdrMode::Scan,
        ]
    }
}

/// What one mode can do, as the API reports it.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModeCapabilities {
    pub mode: SdrMode,
    pub label: &'static str,
    /// "voice", "data", or "voice+data".
    pub kind: &'static str,
    pub description: &'static str,
    pub delivers_audio: bool,
    pub delivers_decode_events: bool,
    pub bandwidth_hz: f32,
    /// How far the filter can be set, for the modes where it can.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<FilterRange>,
    /// Matches the scope-kind byte in the binary FFT frame: "audio",
    /// "symbols", or "mpx".
    pub scope_kind: &'static str,
    pub symbol_rate_hz: f32,
    /// Center of the passband relative to the tuned frequency (sideband).
    pub passband_offset_hz: f32,
    /// Which rolling-capture kinds this mode fills ("iq", "voice",
    /// "discriminator"), so the recorder offers only what it can save.
    pub capture: &'static [&'static str],
}

/// One block of demodulated audio on its way to the browser.
#[derive(Clone, Debug)]
pub struct AudioFrame {
    /// Which receiver: 0 the main one, 1 the second.
    pub rx: u8,
    pub rate_hz: u32,
    /// Mono: what every client gets unless it asked for stereo.
    pub samples: Vec<i16>,
    /// Broadcast FM on receiver A: the stereo decoder's reading, and the
    /// interleaved L, R when it delivered stereo.
    pub stereo: Option<StereoAudio>,
}

/// An audio frame as it goes on the websocket. Binary because a JSON float
/// array costs about ten times the bytes for something that arrives forty
/// times a second. Receiver A's mono is `[0x01][u32 rate][i16…]`, receiver
/// B's `[0x04][u8 rx][u32 rate][i16…]`, which pages that know nothing of it
/// skip. A client that asked for stereo gets broadcast FM as
/// `[0x05][u8 rx][u8 channels][u8 flags][u8 blend][u8 snr_db][u16 pilot_dev_hz][u32 rate][i16…]`,
/// laid out in full beside `usdr_proto::STEREO_AUDIO_MAGIC`.
pub fn encode_audio_frame(f: &AudioFrame, stereo_ok: bool) -> Vec<u8> {
    if stereo_ok && let Some(st) = &f.stereo {
        let two = !st.samples.is_empty();
        let pcm = if two { &st.samples } else { &f.samples };
        let s = &st.status;
        let mut buf = Vec::with_capacity(12 + pcm.len() * 2);
        buf.push(0x05u8);
        buf.push(f.rx);
        buf.push(if two { 2 } else { 1 });
        buf.push(s.pilot as u8);
        buf.push((s.blend.clamp(0.0, 1.0) * 255.0).round() as u8);
        buf.push(s.snr_db.map_or(255, |d| d.round().clamp(0.0, 254.0) as u8));
        buf.extend_from_slice(&s.pilot_dev_hz.map_or(0, |h| h.round().clamp(1.0, 65_535.0) as u16).to_le_bytes());
        buf.extend_from_slice(&f.rate_hz.to_le_bytes());
        for v in pcm {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        return buf;
    }
    let mut buf = Vec::with_capacity(6 + f.samples.len() * 2);
    if f.rx == 0 {
        buf.push(0x01u8);
    } else {
        buf.push(0x04u8);
        buf.push(f.rx);
    }
    buf.extend_from_slice(&f.rate_hz.to_le_bytes());
    for v in &f.samples {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    buf
}

#[derive(Clone, Debug)]
pub struct StereoAudio {
    /// Interleaved L, R, twice the length of the mono; empty while mono.
    pub samples: Vec<i16>,
    pub status: StereoStatus,
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
    /// `None` until the operator chooses: the radio's LO plan then decides
    /// (on for a zero-IF receiver, whose plan costs a few hundred kHz of
    /// span; off for an RTL, whose costs 80% of it).
    #[serde(default)]
    pub lo_offset: Option<bool>,
    #[serde(default)]
    pub clip_guard: bool,
    /// With automatic gain: the tuner's own AGC (VGA included) rather than
    /// the clip-guarded loop.
    #[serde(default)]
    pub tuner_agc: bool,
    /// Voice-scan configuration, applied when the mode is `scan`.
    #[serde(default)]
    pub scan: Option<crate::scan::ScanCfg>,
    /// SoapyRemote hosts to look for an SDRplay receiver on, besides USB.
    #[serde(default)]
    pub remotes: Vec<String>,
    /// SDRplay band-stop filters; `None` leaves the driver's setting.
    #[serde(default)]
    pub rf_notch: Option<bool>,
    #[serde(default)]
    pub dab_notch: Option<bool>,
    #[serde(default)]
    pub processing: Processing,
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
            lo_offset: None,
            clip_guard: false,
            tuner_agc: false,
            scan: None,
            remotes: Vec::new(),
            rf_notch: None,
            dab_notch: None,
            processing: Processing::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct SdrStatus {
    pub serial: String,
    pub tuner: String,
    /// Driver key (`rtlsdr`, `SDRplay`), so the page can label controls the
    /// way this hardware names them.
    #[serde(default)]
    pub driver: String,
    /// Host the radio is on, when it is reached over SoapyRemote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// The range a manual gain is clamped to. Follows the band on an SDRplay,
    /// whose LNA reductions do.
    #[serde(default)]
    pub gain_min_db: f64,
    #[serde(default)]
    pub gain_max_db: f64,
    /// Spans this radio offers, in S/s.
    #[serde(default)]
    pub rates_hz: Vec<f64>,
    /// What the driver accepts, for validating a requested span.
    #[serde(skip_serializing)]
    pub rate_ranges: Vec<(f64, f64)>,
    /// Centre frequencies a tune request may ask for.
    #[serde(default)]
    pub tune_min_hz: f64,
    #[serde(default)]
    pub tune_max_hz: f64,
    /// Front-end switches as the driver reports them; absent where the
    /// hardware has no such switch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bias_t: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rf_notch: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dab_notch: Option<bool>,
    /// SDRplay only: the IF AGC's target level (dBFS) and the RSP's own I/Q
    /// imbalance correction, as the driver reads them back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agc_setpoint_dbfs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iq_correction: Option<bool>,
    /// Where the gain is: an SDRplay's LNA state and IF gain reduction, dB.
    /// Under the RSP's IF AGC the IF is the AGC's own report (absent from a
    /// driver that does not pass it on), never the last value set by hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rfgr_db: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ifgr_db: Option<f64>,
    /// The band's LNA reductions, dB, one per state used.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lna_steps_db: Vec<f64>,
    /// The LNA is stepping on its own, following the IF AGC.
    #[serde(default)]
    pub lna_auto: bool,
    /// SDRplay's power-overload report, now and since the radio opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overload: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overload_count: Option<u64>,
    /// The analog IF filter in front of the ADC, the widths the radio
    /// offers, and whether it was picked by hand rather than from the span.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_bandwidth_hz: Option<f64>,
    #[serde(default)]
    pub if_bandwidths_hz: Vec<f64>,
    #[serde(default)]
    pub if_bandwidth_manual: bool,
    pub freq_hz: f64,
    pub rate_hz: f64,
    pub gain_db: Option<f64>,
    /// Gain the hardware is actually at. Differs from `gain_db` when auto
    /// gain (`gain_db == None`) is walking it under the clip guard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gain_now_db: Option<f64>,
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
    /// Samples this loop knows it never saw (queue overflow between
    /// successful deliveries, from the block sample-position metadata).
    /// Distinct from `lagged_blocks`, which counts lost *deliveries*.
    #[serde(default)]
    pub lost_samples: u64,
    /// Number of discontinuity events (a lost-sample gap the tracker saw).
    #[serde(default)]
    pub gap_events: u64,
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
    /// Automatic gain uses the tuner's own AGC instead of the guarded loop.
    #[serde(default)]
    pub tuner_agc: bool,
    /// The optional receive processing in force.
    #[serde(default)]
    pub processing: Processing,
    /// Display magnification. 1.0 shows the whole usable span.
    #[serde(default)]
    pub zoom: f64,
    /// Zoomed in, the window holds still and the receiver moves across it
    /// (span lock), instead of the window following the receiver. A retune
    /// that leaves the window moves the window onto it.
    #[serde(default)]
    pub zoom_hold: bool,
    /// Width of the channel the inspector is listening through, so the display
    /// can draw the passband it is actually using.
    #[serde(default)]
    pub bandwidth_hz: f64,
    /// Where that passband's center sits from the receive frequency: zero
    /// except in sideband, whose audio is all on one side of the carrier.
    #[serde(default)]
    pub passband_offset_hz: f64,
    /// Receiver-generated spurs being notched, as offsets from the display
    /// centre in Hz.
    #[serde(default)]
    pub spurs_hz: Vec<f64>,
    /// How far the tuned channel's energy actually sits from where the
    /// receiver put it, smoothed. This is what a ppm calibration is measured
    /// from, and what tells the operator the dial is lying.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freq_error_hz: Option<f64>,
    /// PAGER sweep position, when the mode is walking the paging band: which
    /// channel is live and where it sits. Emitted on every dwell step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pager_sweep: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pager_live_hz: Option<f64>,
    /// Voice-scan state, when the mode is walking a configured range. Also
    /// carries the effective scan configuration, so the panel always shows
    /// what the server is actually doing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan: Option<crate::scan::ScanStatus>,
    /// Live FLEX decoder diagnostics when in FLEX mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flex_diag: Option<scannerd_engine::flex::FlexDiagnostics>,
    /// Display averaging: "off", "slow" or "deep".
    #[serde(default)]
    pub avg: String,
    /// The passband edges in use, relative to the carrier, and whether the
    /// operator set them (rather than the mode's own).
    #[serde(default)]
    pub filter_low_hz: f64,
    #[serde(default)]
    pub filter_high_hz: f64,
    #[serde(default)]
    pub filter_custom: bool,
    /// The passband skirt's shape: "sharp", "normal" or "soft".
    #[serde(default)]
    pub filter_shape: FilterShape,
    /// The second receiver, when one is listening.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rx2: Option<crate::sub_rx::SubRxStatus>,
    /// What the broadcast station on receiver A is sending over RDS, once
    /// its PI has been confirmed (WFM only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rds: Option<scannerd_engine::rds::RdsStation>,
    /// The stereo decoder on receiver A (WFM only): pilot, blend, and the
    /// S/N it is blending on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stereo: Option<StereoStatus>,
    /// The HF text and picture decoders' settings, and the running one's
    /// reading (WEFAX, RTTY, NAVTEX, PSK31 only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digital_cfg: Option<usdr_proto::DigitalCfg>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digital: Option<usdr_proto::DigitalReading>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeakMarker {
    pub freq_hz: f64,
    /// Internal only (sort + dedupe); the client never renders it.
    #[serde(skip_serializing)]
    pub pwr_db: f32,
    pub snr_db: f32,
    /// Protocol last decoded at this frequency, if any. Stamped from the
    /// decode history so the spectrum shows what kind of signal each marker
    /// is, not just that it is there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Frequencies that have actually produced a decode, newest protocol wins.
///
/// Feeds peak labels: a marker labelled "POCSAG" earned that name from a real
/// frame, not a heuristic fit to a bump in the spectrum. Entries expire so a
/// signal that left the air does not stay labelled forever.
pub(crate) struct DecodeHistory {
    labels: std::collections::HashMap<u64, (String, Instant)>,
    last: Instant,
}

const LABEL_TTL: Duration = Duration::from_secs(600);
/// How far a peak may sit from a labelled frequency and keep the label.
const LABEL_MATCH_KHZ: u64 = 3;

impl DecodeHistory {
    pub(crate) fn new() -> Self {
        Self {
            labels: std::collections::HashMap::new(),
            last: Instant::now(),
        }
    }

    pub(crate) fn note(&mut self, hz: f64, protocol: &str) {
        if protocol.is_empty() {
            return;
        }
        self.last = Instant::now();
        let key = (hz / 1000.0).round() as u64;
        self.labels.insert(key, (protocol.to_string(), Instant::now()));
    }

    fn label_for(&self, hz: f64) -> Option<String> {
        let base = (hz / 1000.0).round() as i64;
        let mut best_seen: Option<Instant> = None;
        let mut best_label: Option<String> = None;
        for d in -(LABEL_MATCH_KHZ as i64)..=(LABEL_MATCH_KHZ as i64) {
            let key = (base + d).max(0) as u64;
            if let Some((proto, seen)) = self.labels.get(&key) {
                if best_seen.is_none_or(|b| *seen > b) {
                    best_seen = Some(*seen);
                    best_label = Some(proto.clone());
                }
            }
        }
        let _ = best_seen;
        best_label.filter(|_| {
            best_seen.is_some_and(|seen| seen.elapsed() < LABEL_TTL)
        })
    }

    fn prune(&mut self) {
        self.labels.retain(|_, (_, seen)| seen.elapsed() < LABEL_TTL);
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SdrEvent {
    #[serde(rename_all = "camelCase")]
    Fft {
        center_hz: f64,
        rate_hz: f64,
        // min/max dB are deliberately not shipped: the client computes its own
        // auto range, so they were pure dead weight at 25 fps.
        /// Power in dB for each FFT bin (size = fft_size).
        pwr: Vec<f32>,
        /// Slowly-decaying max-hold trace, for a "what has been there" line.
        #[serde(default)]
        max_hold: Vec<f32>,
        /// Strongest segment in each bin over this frame's interval, for a
        /// waterfall that shows bursts at full level.
        #[serde(default)]
        wf_peak: Vec<f32>,
        /// This frame's own Welch mean per bin, before the display's bin
        /// smoothing and AVG, so the waterfall shows what was measured.
        #[serde(default)]
        wf_row: Vec<f32>,
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
    /// The binary FFT frame, encoded once on the DSP thread (see
    /// `encode_fft_frame`). Every websocket client sends identical bytes, so
    /// encoding per client — after the broadcast had already deep-cloned the
    /// whole `Fft` payload per client — was cost multiplied by the client
    /// count. Never serialized: the socket path ships it as a binary message
    /// before the JSON arm can see it.
    FftBytes {
        /// Shared, not copied: the broadcast hands every subscriber a clone
        /// of the event, and the socket a clone of the frame, which for a
        /// `Vec` was two copies of the whole frame per client per frame.
        #[serde(skip)]
        frame: axum::body::Bytes,
    },
    /// One radiofax row as its 0x06 frame, for the desktop clients only;
    /// like `FftBytes`, never serialized.
    FaxRow {
        #[serde(skip)]
        frame: axum::body::Bytes,
    },
}

/// Wire format for binary FFT frames, shared by `encode_fft_frame` and the
/// browser's decoder. Version byte first so a future layout change can bump
/// it instead of breaking every client on reload.
pub const FFT_FRAME_MAGIC: u8 = 0x03;

fn scope_kind_byte(kind: &str) -> u8 {
    match kind {
        "symbols" => 1,
        "mpx" => 2,
        _ => 0,
    }
}

/// Serialise an FFT event to the compact binary frame `ws_client` ships and
/// the browser decodes with DataView + typed arrays. Returns `None` for
/// non-FFT events (the caller routes those through JSON).
///
/// A JSON float array costs ~10 bytes per bin; this costs exactly four, so at
/// fft_size 8192 the frame drops from ~80 KB of text to ~40 KB of binary —
// and the client stops tokenising all of it on the main thread.
/// Classification, flattened into the binary FFT frame. `None` is a flag
/// byte of zero; the numbers that follow are then meaningless placeholders.
struct FrameClassification {
    active: bool,
    snr_db: f32,
    rf_dbfs: f32,
    peak_dev_hz: f32,
    rms_dev_hz: f32,
    center_offset_hz: f32,
    confidence: f32,
    protocol: String,
    modulation: String,
    details: Option<String>,
}

impl From<&ClassificationResult> for FrameClassification {
    fn from(c: &ClassificationResult) -> Self {
        Self {
            active: c.active,
            snr_db: c.snr_db,
            rf_dbfs: c.rf_dbfs,
            peak_dev_hz: c.peak_dev_hz,
            rms_dev_hz: c.rms_dev_hz,
            center_offset_hz: c.center_offset_hz,
            confidence: c.confidence,
            protocol: c.protocol.clone(),
            modulation: c.modulation.clone(),
            details: c.details.clone(),
        }
    }
}

fn encode_classification(buf: &mut Vec<u8>, c: Option<&FrameClassification>) {
    match c {
        Some(c) => {
            buf.push(1);
            buf.push(u8::from(c.active));
            buf.extend_from_slice(&c.snr_db.to_le_bytes());
            buf.extend_from_slice(&c.rf_dbfs.to_le_bytes());
            buf.extend_from_slice(&c.peak_dev_hz.to_le_bytes());
            buf.extend_from_slice(&c.rms_dev_hz.to_le_bytes());
            buf.extend_from_slice(&c.center_offset_hz.to_le_bytes());
            buf.extend_from_slice(&c.confidence.to_le_bytes());
            // Three strings, each u8-length-prefixed like the peak labels.
            let mut put = |s: &str| {
                let bytes = s.as_bytes();
                buf.push(bytes.len().min(255) as u8);
                buf.extend_from_slice(&bytes[..bytes.len().min(255)]);
            };
            put(&c.protocol);
            put(&c.modulation);
            put(c.details.as_deref().unwrap_or(""));
        }
        None => buf.push(0),
    }
}

pub fn encode_fft_frame(ev: &SdrEvent) -> Option<Vec<u8>> {
    let SdrEvent::Fft {
        center_hz,
        rate_hz,
        pwr,
        max_hold,
        wf_peak,
        wf_row,
        peak_iq,
        inspect_hz,
        classification,
        peaks,
        scope,
        scope_rate_hz,
        scope_kind,
        symbol_rate_hz,
        channel_dbfs,
        noise_dbfs,
    } = ev
    else {
        return None;
    };
    let classification = classification.as_ref().map(FrameClassification::from);
    let mut buf = Vec::with_capacity(
        56 + (pwr.len() + max_hold.len() + wf_peak.len() + wf_row.len()) * 4 + scope.len() * 2 + peaks.len() * 16,
    );
    buf.push(FFT_FRAME_MAGIC);
    buf.extend_from_slice(&center_hz.to_le_bytes());
    buf.extend_from_slice(&rate_hz.to_le_bytes());
    buf.extend_from_slice(&peak_iq.to_le_bytes());
    buf.extend_from_slice(&inspect_hz.to_le_bytes());
    // Scope rate stored in steps of 10 Hz as u16 (supports up to 655.35 kHz).
    // The previous `* 10.0` saturated u16 for any rate >= 6553.5 Hz (e.g. 48 kHz
    // and 128 kHz both pinned at 65535, reading as 6553.5 Hz in the browser).
    buf.extend_from_slice(
        &(((scope_rate_hz / 10.0).round() as u32).min(u16::MAX as u32) as u16).to_le_bytes(),
    );
    buf.push(scope_kind_byte(scope_kind));
    buf.extend_from_slice(&((symbol_rate_hz * 10.0) as u16).to_le_bytes());
    buf.extend_from_slice(&channel_dbfs.to_le_bytes());
    buf.extend_from_slice(&noise_dbfs.to_le_bytes());
    // Scope rides along: same i16 samples the JSON path shipped.
    buf.extend_from_slice(&(scope.len() as u32).to_le_bytes());
    for s in scope {
        buf.extend_from_slice(&s.to_le_bytes());
    }
    buf.extend_from_slice(&(pwr.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(max_hold.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(peaks.len() as u32).to_le_bytes());
    for &v in pwr {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    for &v in max_hold {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    for p in peaks {
        buf.extend_from_slice(&p.freq_hz.to_le_bytes());
        buf.extend_from_slice(&p.snr_db.to_le_bytes());
        let label = p.label.as_deref().unwrap_or("");
        // The length is one u8 on the wire; truncate like the classification
        // block does so a long label can never desynchronise the frame.
        let bytes = label.as_bytes();
        let n = bytes.len().min(255);
        buf.push(n as u8);
        buf.extend_from_slice(&bytes[..n]);
    }
    // v3 appends the classification block last (see encode_classification).
    encode_classification(&mut buf, classification.as_ref());
    // v4: an optional per-frame peak array for the waterfall, after
    // everything a v3 reader knows. A v3 reader stops before it.
    // v5: the unsmoothed row after it. The peak count is then written
    // even when empty, so a v4 reader takes a zero-length peak array and
    // stops before the row.
    if !wf_peak.is_empty() || !wf_row.is_empty() {
        buf.extend_from_slice(&(wf_peak.len() as u32).to_le_bytes());
        for &v in wf_peak {
            buf.extend_from_slice(&v.to_le_bytes());
        }
    }
    if !wf_row.is_empty() {
        buf.extend_from_slice(&(wf_row.len() as u32).to_le_bytes());
        for &v in wf_row {
            buf.extend_from_slice(&v.to_le_bytes());
        }
    }
    Some(buf)
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
    /// Raw device span at `span_rate`, interleaved as I/Q. Twelve seconds
    /// rolling, capped at `SPAN_MAX_SAMPLES` — at 2 MS/s that is 96 MiB,
    /// plus a clamp-and-convert pass over every sample, so the ring only
    /// runs while something wants it: the `USDR_PAGE_DUMP` feature, or a window after a span capture
    /// was last downloaded (nothing in the UI asks for it — it is a curl
    /// diagnostic). See `span_armed`.
    span: VecDeque<(i16, i16)>,
    span_rate: f64,
    span_armed_until: Option<std::time::Instant>,
}

impl CaptureBuffer {
    fn clear(&mut self, inspect_hz: f64, fs_hz: f64) {
        self.inspect_hz = inspect_hz;
        self.fs_hz = fs_hz;
        self.voice.clear();
        self.discriminator.clear();
        self.iq.clear();
    }

    /// Most raw span samples the ring holds: 96 MiB as i16 pairs, twelve
    /// seconds up to 2 MS/s and three at 8 MS/s. A download holds the ring,
    /// an interleaved copy and the WAV at once, so twelve seconds at 8 MS/s
    /// (384 MiB each) was three times what the daemon's 512 MiB memory limit
    /// allows, and the download killed it.
    const SPAN_MAX_SAMPLES: usize = 24 << 20;

    /// How long a span download keeps the raw ring filling after the request.
    /// Generous enough to re-download the full 12 s once it has rolled in.
    const SPAN_ARM_SECS: u64 = 180;

    /// USDR_PAGE_DUMP needs the raw span resident at all times — the dump
    /// happens the moment a page decodes. Checked once; it is an env var.
    fn page_dump_on() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("USDR_PAGE_DUMP").is_some())
    }

    fn span_armed(&self) -> bool {
        self.span_armed_until.is_some_and(|t| std::time::Instant::now() < t)
            || Self::page_dump_on()
    }

    fn arm_span(&mut self) {
        self.span_armed_until =
            Some(std::time::Instant::now() + std::time::Duration::from_secs(Self::SPAN_ARM_SECS));
    }

    /// Append raw span samples to the rolling raw capture (~12 s at the span
    /// rate). Called from the device loop before any processing; a no-op
    /// while the ring is not armed — the rate is still tracked so a capture
    /// taken mid-fill is labelled with the true span rate.
    ///
    /// `discontinuous` marks a break in the sample timeline (a lost-delivery
    /// gap or an acquisition epoch boundary): the ring is dropped rather than
    /// spliced across a hole, so a downloaded capture never stitches samples
    /// that were never adjacent on the air.
    fn append_span(&mut self, rate: f64, block: &[num_complex::Complex32], discontinuous: bool) {
        if (self.span_rate - rate).abs() >= 1.0 || discontinuous {
            self.span.clear();
            self.span_rate = rate;
        }
        if !self.span_armed() {
            return;
        }
        self.span.extend(block.iter().map(|x| {
            (
                (x.re.clamp(-1.0, 1.0) * 32_000.0) as i16,
                (x.im.clamp(-1.0, 1.0) * 32_000.0) as i16,
            )
        }));
        let max = (rate.max(1.0) as usize * 12).min(Self::SPAN_MAX_SAMPLES);
        while self.span.len() > max {
            self.span.drain(..(self.span.len() - max));
        }
    }

    fn span_rate(&self) -> u32 {
        if self.span_rate > 0.0 {
            self.span_rate.round() as u32
        } else {
            0
        }
    }

    /// Snapshot the raw span ring and the discriminator ring (the decoder's
    /// actual input) as interleaved i16 samples plus their rates. Diagnostic:
    /// called on a decoded page so the capture is guaranteed to contain the
    /// frame that produced it.
    ///
    /// Returns samples rather than finished WAV bytes so the caller can
    /// release the capture lock before serialising: this runs in the radio
    /// loop, and a multi-megabyte WAV encode under the mutex stalls the DSP
    /// thread for the whole encode — the same fault `capture_wav` was fixed
    /// for.
    fn page_debug_samples(&self) -> (Vec<i16>, u32, Vec<i16>, u32) {
        let mut span: Vec<i16> = Vec::with_capacity(self.span.len() * 2);
        for &(i, q) in &self.span {
            span.extend([i, q]);
        }
        let disc: Vec<i16> = self.discriminator.iter().copied().collect();
        (span, self.span_rate().max(8_000), disc, self.rate().max(8_000))
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
    TunerAgc(bool),
    BiasT(bool),
    RfNotch(bool),
    DabNotch(bool),
    AgcSetpoint(f64),
    IqCorrection(bool),
    /// Analog IF filter width; `None` follows the span again.
    IfBandwidth(Option<f64>),
    /// Offsets from the display centre, in Hz, that a spur check identified as
    /// generated inside the receiver.
    Spurs(Vec<f64>),
    Ppm(f64),
    Processing(Processing),
    Zoom(f64),
    /// Hold the zoomed window still (at `center_hz`, or where it is now) or
    /// let it follow the receiver again.
    ZoomHold { hold: bool, center_hz: Option<f64> },
    /// About how many bins the browser's spectrum is wide at 1x zoom, so the
    /// FFT resolves what the screen can show and no more.
    DisplayBins(usize),
    /// Display averaging mode for the spectrum (weak-signal aid).
    Avg(AvgMode),
    /// The current mode's filter was set or cleared (already in the table):
    /// rebuild the chain around it.
    Filter,
    /// Start, move or (with `None`) stop the second receiver.
    Rx2(Option<(f64, SdrMode)>),
    /// Replace the voice-scan configuration (range, modes, threshold).
    ScanConfig(crate::scan::ScanCfg),
    /// Pause/resume/skip/forget/unskip, from the scan panel. `Unskip` names
    /// the frequency to re-test (matched ±2 kHz) in the payload.
    ScanControl(crate::scan::ScanControl, Option<f64>),
    /// The HF text and picture decoders' settings changed (already in
    /// `hfdata`'s table), or a radiofax picture is to start or stop now.
    Digital { fax_start: bool, fax_stop: bool },
}

/// Frame-to-frame integration of the displayed spectrum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AvgMode {
    Off,
    /// ~0.5 s time constant: steady carriers emerge from the flicker.
    Slow,
    /// ~2 s: for beacons and marginal carriers.
    Deeper,
}

/// Put a mode's passband in the status.
fn stamp_filter(s: &mut SdrStatus, mode: SdrMode) {
    let (lo, hi) = mode.filter_edges();
    s.bandwidth_hz = f64::from(hi - lo);
    s.passband_offset_hz = f64::from((lo + hi) / 2.0);
    s.filter_low_hz = f64::from(lo);
    s.filter_high_hz = f64::from(hi);
    s.filter_custom = filter_override(mode).is_some();
    s.filter_shape = mode.filter_shape();
}

impl AvgMode {
    /// The name `/api/sdr/avg` takes and the status reports.
    pub fn wire(self) -> &'static str {
        match self {
            AvgMode::Off => "off",
            AvgMode::Slow => "slow",
            AvgMode::Deeper => "deep",
        }
    }
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

    pub fn scan_config(&self, cfg: crate::scan::ScanCfg) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::ScanConfig(cfg))
            .map_err(|e| anyhow::anyhow!("send scan config: {e}"))
    }

    pub fn scan_control(&self, ctl: crate::scan::ScanControl, freq_hz: Option<f64>) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::ScanControl(ctl, freq_hz))
            .map_err(|e| anyhow::anyhow!("send scan control: {e}"))
    }

    pub fn set_display_bins(&self, bins: usize) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::DisplayBins(bins))
            .map_err(|e| anyhow::anyhow!("send display bins: {e}"))
    }

    pub fn set_zoom(&self, zoom: f64) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::Zoom(zoom))
            .map_err(|e| anyhow::anyhow!("send zoom: {e}"))
    }

    pub fn set_zoom_hold(&self, hold: bool, center_hz: Option<f64>) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::ZoomHold { hold, center_hz })
            .map_err(|e| anyhow::anyhow!("send zoom hold: {e}"))
    }

    /// Display averaging mode (weak-signal spectrum aid).
    /// The current mode's filter changed (see [`SdrMode::set_filter`]).
    pub fn set_rx2(&self, rx: Option<(f64, SdrMode)>) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::Rx2(rx))
            .map_err(|e| anyhow::anyhow!("send rx2: {e}"))
    }

    pub fn filter_changed(&self) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::Filter)
            .map_err(|e| anyhow::anyhow!("send filter: {e}"))
    }

    pub fn set_avg(&self, mode: AvgMode) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::Avg(mode))
            .map_err(|e| anyhow::anyhow!("send avg: {e}"))
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

    pub fn set_tuner_agc(&self, on: bool) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::TunerAgc(on))
            .map_err(|e| anyhow::anyhow!("send tuner agc: {e}"))
    }

    pub fn set_bias_t(&self, on: bool) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::BiasT(on))
            .map_err(|e| anyhow::anyhow!("send bias-T: {e}"))
    }

    pub fn set_rf_notch(&self, on: bool) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::RfNotch(on))
            .map_err(|e| anyhow::anyhow!("send RF notch: {e}"))
    }

    /// The HF decoders' settings are in `hfdata`'s table; tell the running
    /// one, and start or stop a radiofax picture.
    pub fn digital(&self, fax_start: bool, fax_stop: bool) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::Digital { fax_start, fax_stop })
            .map_err(|e| anyhow::anyhow!("send digital: {e}"))
    }

    pub fn set_processing(&self, p: Processing) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::Processing(p))
            .map_err(|e| anyhow::anyhow!("send processing: {e}"))
    }

    pub fn set_dab_notch(&self, on: bool) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::DabNotch(on))
            .map_err(|e| anyhow::anyhow!("send DAB notch: {e}"))
    }

    pub fn set_agc_setpoint(&self, dbfs: f64) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::AgcSetpoint(dbfs))
            .map_err(|e| anyhow::anyhow!("send AGC setpoint: {e}"))
    }

    pub fn set_iq_correction(&self, on: bool) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::IqCorrection(on))
            .map_err(|e| anyhow::anyhow!("send I/Q correction: {e}"))
    }

    pub fn set_if_bandwidth(&self, hz: Option<f64>) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::IfBandwidth(hz))
            .map_err(|e| anyhow::anyhow!("send IF filter: {e}"))
    }

    pub fn switch_device(&self, serial: String) -> Result<()> {
        self.cmd_tx
            .send(SdrCmd::SwitchDevice(serial))
            .map_err(|e| anyhow::anyhow!("send switch: {e}"))
    }

    pub fn capture_wav(&self, kind: CaptureKind) -> (f64, usize, Vec<u8>) {
        // Snapshot under the lock, encode outside it: the DSP thread appends
        // to this buffer every block, and holding the mutex through a
        // multi-MiB WAV serialisation stalled it for the whole download.
        let (inspect_hz, rate, samples) = {
            let mut capture = self.capture.lock().expect("SDR capture");
            let rate = match kind {
                CaptureKind::Span => capture.span_rate(),
                _ => capture.rate(),
            };
            let samples = match kind {
                CaptureKind::Voice => capture.voice.iter().copied().collect::<Vec<i16>>(),
                CaptureKind::Discriminator => {
                    capture.discriminator.iter().copied().collect::<Vec<i16>>()
                }
                CaptureKind::Iq => {
                    let mut samples = Vec::with_capacity(capture.iq.len() * 2);
                    for &(i, q) in &capture.iq {
                        samples.extend([i, q]);
                    }
                    samples
                }
                CaptureKind::Span => {
                    // The raw ring is demand-armed (see `span_armed`); the
                    // first request of a quiet session starts the fill, so
                    // say so rather than hand back a mysteriously empty WAV.
                    if !capture.span_armed() {
                        capture.arm_span();
                        eprintln!(
                            "SDR: span capture armed — the ring fills at the span rate; \
                             re-request for the full 12 s"
                        );
                    }
                    let mut samples = Vec::with_capacity(capture.span.len() * 2);
                    for &(i, q) in &capture.span {
                        samples.extend([i, q]);
                    }
                    samples
                }
            };
            (capture.inspect_hz, rate, samples)
        };
        let count = match kind {
            CaptureKind::Iq | CaptureKind::Span => samples.len() / 2,
            _ => samples.len(),
        };
        let channels = matches!(kind, CaptureKind::Iq | CaptureKind::Span)
            .then_some(2)
            .unwrap_or(1);
        (inspect_hz, count, pcm_wav(&samples, rate, channels))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureKind {
    Voice,
    Discriminator,
    Iq,
    /// Raw device span, before any filtering — what the tuner actually
    /// delivered, at the span rate. Diagnostic: unlike the inspected I/Q this
    /// has not been through the channel chain, so a signal that decodes but
    /// cannot be seen here is being manufactured (or lost) downstream.
    Span,
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
///
/// Scan declines the offset at any width: its peak hunt runs over the raw
/// full-span spectrum, whose bins are laid out around the hardware LO, so an
/// active offset would shift every candidate by `rate * 0.125` and the slots
/// would tune to empty air.
/// Which LO-offset plan the open radio needs (see [`set_lo_plan`]).
static LO_PLAN_ZERO_IF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Pick the LO-offset plan for the radio just opened.
///
/// An RTL-SDR has a spur at ±fs/4 as well as DC, so its plan parks the
/// window between the two and keeps only [`LO_USABLE_FRACTION`] of the span.
/// A zero-IF receiver with no such spur (the SDRplay RSPs) only has to keep
/// DC out of the channel: its plan parks the LO just past the channel and
/// shows everything else, about 4.8 MHz of a 5 MS/s span instead of 1 MHz.
fn set_lo_plan(caps: &scannerd_radio::Capabilities) {
    let zero_if = caps.driver.to_ascii_lowercase().contains("sdrplay");
    LO_PLAN_ZERO_IF.store(zero_if, std::sync::atomic::Ordering::Relaxed);
}

fn lo_plan_zero_if() -> bool {
    LO_PLAN_ZERO_IF.load(std::sync::atomic::Ordering::Relaxed)
}

/// The LO offset on the zero-IF plan: the passband's far edge plus 50 kHz,
/// never under 100 kHz, never over 15% of the span (narrow spans give up
/// more of their width; from 1 MS/s up the cap never binds). Measured on the
/// RSP1A at 5 MS/s, the DC residue that survives both its own correction and
/// the software blocker is a hump 8 dB over the floor at the LO and still
/// 1 dB up 15 kHz out; anything nearer the LO than that is listened to
/// through it.
fn zero_if_offset(rate_hz: f64, mode: SdrMode) -> f64 {
    let edge = f64::from(mode.default_bandwidth_hz()) / 2.0 + f64::from(mode.default_passband_offset_hz()).abs();
    (edge + 50e3).max(100e3).min(rate_hz * 0.15)
}

fn lo_active(rate_hz: f64, on: bool, mode: SdrMode) -> bool {
    if !on || matches!(mode, SdrMode::Scan) {
        return false;
    }
    if lo_plan_zero_if() {
        // DC must land outside the passband with room to spare.
        let half = f64::from(mode.default_bandwidth_hz()) / 2.0 + f64::from(mode.default_passband_offset_hz()).abs();
        zero_if_offset(rate_hz, mode) >= half + (rate_hz * 0.005).max(5e3)
    } else {
        f64::from(mode.default_bandwidth_hz()) <= rate_hz * LO_USABLE_FRACTION / 3.0
    }
}

/// LO offset for a span, or zero when the offset is off or does not fit.
fn lo_offset_for(rate_hz: f64, on: bool, mode: SdrMode) -> f64 {
    if !lo_active(rate_hz, on, mode) {
        0.0
    } else if lo_plan_zero_if() {
        zero_if_offset(rate_hz, mode)
    } else {
        rate_hz * LO_OFFSET_FRACTION
    }
}

/// How far either side of the LO the display notch blanks: the RSP's DC
/// residue is a hump about ±15 kHz wide (see [`zero_if_offset`]).
const DC_NOTCH_HZ: f64 = 15_000.0;

/// How much of each end of the sampled band the tuner's IF filter rolls off:
/// at 8 MS/s with the RSP1A's 8 MHz filter the noise floor is flat from about
/// 420 kHz in from either edge. A zoomed window is placed inside that.
const EDGE_SKIRT_FRACTION: f64 = 0.06;

/// Where to put the tuner so the LO's DC residue, and the display notch
/// over it, stay out of a zoomed window. `None` when they already do, when
/// there is nothing to move (no offset, not zoomed, an RTL's plan whose
/// window never holds DC), or when no placement keeps the receivers in the
/// span.
///
/// At 1x the residue is always somewhere in the span: notched, 30 kHz of
/// several MHz. Zoomed onto a receiver away from the tuner, it can land in
/// the middle of the window, a dead band where a signal may be. Moving the
/// tuner so DC sits just past the window's edge (on the side it is on now,
/// and never nearer the receiver than the plan's own offset) costs one
/// retune and hides nothing that is being looked at.
fn lo_clear_freq(rate_hz: f64, lo_on: bool, mode: SdrMode, zoom: f64, current_freq: f64, view_hz: f64, receivers: &[f64]) -> Option<f64> {
    let lo_off = lo_offset_for(rate_hz, lo_on, mode);
    if lo_off == 0.0 || zoom <= 1.0 || !lo_plan_zero_if() {
        return None;
    }
    // The window is where it was asked to be (not clamped against the span's
    // edge) and clear of the tuner's filter skirt.
    let fits = |freq: f64| {
        let w = ShownWindow::new(rate_hz, lo_on, mode, zoom, freq, view_hz);
        (w.shown_centre - view_hz).abs() < 1.0
            && (view_hz - (freq + lo_off)).abs() + w.shown_rate / 2.0 <= rate_hz / 2.0 - rate_hz * EDGE_SKIRT_FRACTION
    };
    let clear = |freq: f64| {
        let w = ShownWindow::new(rate_hz, lo_on, mode, zoom, freq, view_hz);
        fits(freq) && (freq + lo_off - w.shown_centre).abs() > w.shown_rate / 2.0 + DC_NOTCH_HZ
    };
    if clear(current_freq) {
        return None;
    }
    let display = display_rate_for(rate_hz, lo_on, mode);
    let keeps = |f: f64| receivers.iter().all(|r| (r - f).abs() <= display * 0.45);
    let half = display / zoom / 2.0;
    let margin = (display / zoom * 0.1).max(10e3);
    let d = (half + DC_NOTCH_HZ + margin).max(lo_off);
    let side = if current_freq + lo_off >= view_hz { 1.0 } else { -1.0 };
    if let Some(f) = [side, -side].into_iter().map(|s| view_hz + s * d - lo_off).find(|&f| clear(f) && keeps(f)) {
        return Some(f);
    }
    // No placement keeps DC out (a wide window, 2x): the notch stays in view
    // as it does at 1x. Pushing DC out anyway shoved the window against the
    // span's edge, its first quarter the filter's skirt and the receiver off
    // center. If an earlier move left it there, put the tuner back on the
    // receiver.
    if fits(current_freq) {
        return None;
    }
    let f = view_hz - lo_off;
    (fits(f) && keeps(f)).then_some(f)
}

/// What a mode change does to the LO.
#[derive(Debug, PartialEq, Eq)]
enum LoMove {
    /// The offset comes on or goes off: only a reopen re-negotiates the
    /// analog IF width it needs.
    Reopen,
    /// Same plan, another distance. The zero-IF offset follows the mode's
    /// width (WFM parks the LO 150 kHz out, the narrow modes 100 kHz), and
    /// the channel is mixed for the new one, so the tuner has to follow.
    /// Without it, tuning in NFM and switching to WFM left the station 50 kHz
    /// off in the channel: +0.6 of full scale of DC in the audio, its peaks
    /// clipped, and the spectrum's axis 50 kHz out.
    Retune,
    Stay,
}

fn lo_move(rate_hz: f64, on: bool, from: SdrMode, to: SdrMode) -> LoMove {
    lo_move_between((rate_hz, from), (rate_hz, to), on)
}

/// What going from one span and mode to another does to the LO. The span
/// counts as much as the mode: under 1 MS/s the zero-IF offset is capped at
/// 15% of the span (and WFM's no longer fits at all), and the other plan's
/// is a fraction of the span at every rate. A span change that left the
/// tuner where the old offset had put it had the same fault a mode change
/// used to: the channel mixed for one distance, the tuner at another.
fn lo_move_between(from: (f64, SdrMode), to: (f64, SdrMode), on: bool) -> LoMove {
    if lo_active(from.0, on, from.1) != lo_active(to.0, on, to.1) {
        LoMove::Reopen
    } else if lo_offset_for(from.0, on, from.1) != lo_offset_for(to.0, on, to.1) {
        LoMove::Retune
    } else {
        LoMove::Stay
    }
}

/// The span actually shown, which shrinks when the LO is parked off-centre:
/// centered on the receive frequency, it can only reach as far as the
/// nearer edge of the sampled band.
fn display_rate_for(rate_hz: f64, lo_on: bool, mode: SdrMode) -> f64 {
    if !lo_active(rate_hz, lo_on, mode) {
        rate_hz
    } else if lo_plan_zero_if() {
        rate_hz - 2.0 * zero_if_offset(rate_hz, mode)
    } else {
        rate_hz * LO_USABLE_FRACTION
    }
}

/// The SDRplay band-stop filters as the operator left them, applied at every
/// open so a reopen after a fault comes back the same.
#[derive(Clone, Copy, Debug, Default)]
struct Notches {
    rf: Option<bool>,
    dab: Option<bool>,
}

/// Where a radio was found: on this machine's USB, or on a SoapyRemote host.
fn where_searched(remotes: &[String]) -> String {
    if remotes.is_empty() {
        "on USB".into()
    } else {
        let hosts: Vec<&str> = remotes.iter().map(|r| device::remote_host(r)).collect();
        format!("on USB or on {}", hosts.join(", "))
    }
}

/// Opens the radio with `serial`, or the first one found: local dongles
/// first, then each remote. Returns the device, its serial and tuner name,
/// and the host it is on when remote.
/// The LO-offset setting as the opener needs it: whether the operator has
/// chosen (if not, the radio's plan decides and the choice is written back).
struct LoRequest<'a> {
    on: &'a mut bool,
    chosen: bool,
    mode: SdrMode,
}

fn open_sdr(
    remotes: &[String],
    serial: Option<&str>,
    freq_hz: f64,
    rate_hz: f64,
    gain_db: Option<f64>,
    lo: LoRequest<'_>,
    notches: Notches,
) -> Result<(Device, String, String, Option<String>)> {
    let all = device::enumerate(remotes)?;
    if all.is_empty() {
        bail!("no radio found {}", where_searched(remotes));
    }
    let info = if let Some(s) = serial {
        all.iter()
            .find(|d| d.serial == s)
            .with_context(|| format!("SDR serial {s} not found {}", where_searched(remotes)))?
    } else {
        all.first().unwrap()
    };
    // The LO plan belongs to the radio, so it is settled before the first
    // tune: zero-IF for an SDRplay, wherever it is reached from.
    LO_PLAN_ZERO_IF.store(info.driver == device::SDRPLAY, std::sync::atomic::Ordering::Relaxed);
    if !lo.chosen {
        *lo.on = lo_plan_zero_if();
    }
    let lo_offset_hz = lo_offset_for(rate_hz, *lo.on, lo.mode);
    let mut cfg = DeviceConfig::new(&info.serial, freq_hz + lo_offset_hz, rate_hz);
    cfg.gain = gain_db;
    let cal = config::load().unwrap_or_default();
    cfg.ppm = cal.ppm_for(&info.serial);
    cfg.bias_t = cal.bias_t_for(&info.serial);
    cfg.remote = info.remote.clone();
    cfg.driver = info.driver;
    cfg.rf_notch = notches.rf;
    cfg.dab_notch = notches.dab;
    // With the LO parked off-centre the analog IF has to reach past the
    // offset, or the tuner filters away the half of the span it was moved
    // toward. Zero asks the driver for its widest.
    cfg.cover_hz = if lo_offset_hz != 0.0 && !lo_plan_zero_if() {
        scannerd_radio::cover_hz_offset(lo_offset_hz, rate_hz * LO_USABLE_FRACTION)
    } else {
        0.0
    };
    cfg.role = Role::Sdr;
    let dev = device::open(&cfg)?;
    let host = info.remote.as_deref().map(|r| device::remote_host(r).to_string());
    Ok((dev, info.serial.clone(), info.tuner.clone(), host))
}

/// Spans an RTL-SDR offers: what the page has always offered.
const RTL_SPANS_HZ: [f64; 6] = [250_000.0, 1_024_000.0, 1_536_000.0, 2_048_000.0, 2_400_000.0, 3_200_000.0];

/// Spans an SDRplay offers. Its narrow spans are the rates it decimates to in
/// hardware (250k, 500k, 1M) rather than the RTL's, which it does not have,
/// and the top is the RSP's own 10 MS/s. Measured against the RSP1A over
/// SoapyRemote, the worker thread ran at 20% of a core at 6 MS/s with no
/// dropped, lagged or lost samples.
const SDRPLAY_SPANS_HZ: [f64; 11] = [
    250_000.0, 500_000.0, 1_000_000.0, 2_000_000.0, 2_048_000.0, 3_000_000.0, 4_000_000.0,
    5_000_000.0, 6_000_000.0, 8_000_000.0, 10_000_000.0,
];

fn span_rates(caps: &scannerd_radio::Capabilities) -> Vec<f64> {
    let offered: &[f64] = if matches!(caps.gain, scannerd_radio::GainControl::Sdrplay { .. }) {
        &SDRPLAY_SPANS_HZ
    } else {
        &RTL_SPANS_HZ
    };
    offered
        .iter()
        .copied()
        .filter(|&r| caps.rate_ranges.is_empty() || caps.supports_rate(r))
        .collect()
}

/// Tune requests are held to 1 MHz – 2.2 GHz, a sanity bound that does not
/// pretend to know the tuner. A radio that reaches lower — an RSP goes down
/// to 1 kHz, which is all of LF and MF — lowers the floor to its own.
fn tune_bounds(caps: &scannerd_radio::Capabilities) -> (f64, f64) {
    let lo = caps.freq_range.map_or(1e6, |(lo, _)| lo.clamp(1e3, 1e6));
    (lo, 2.2e9)
}

/// The status fields that describe the hardware rather than its tuning.
/// Gain range and the front-end switches follow with the device's first
/// state report.
fn describe_radio(s: &mut SdrStatus, dev: &Device, host: Option<String>) {
    set_lo_plan(&dev.caps);
    s.driver = dev.caps.driver.clone();
    s.host = host;
    s.rates_hz = span_rates(&dev.caps);
    s.rate_ranges = dev.caps.rate_ranges.clone();
    s.if_bandwidths_hz = dev.caps.if_bandwidths.clone();
    (s.tune_min_hz, s.tune_max_hz) = tune_bounds(&dev.caps);
}

pub fn spawn(
    cfg: SdrCfg,
    events: broadcast::Sender<SdrEvent>,
    audio: broadcast::Sender<AudioFrame>,
    calls: Arc<Mutex<VecDeque<RecordedCall>>>,
) -> Result<SdrRuntime> {
    let status = Arc::new(Mutex::new(SdrStatus::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    let capture = Arc::new(Mutex::new(CaptureBuffer::default()));

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
    let mut chain = DecodeChain::new(rate_hz, mode.chain_bandwidth_hz(), mode.target_rate());
    chain.set_offset(offset_hz);
    let mut classifier = SignalClassifier::new(chain.fs_out());
    if mode == SdrMode::Flex {
        classifier.set_flex_only(true);
    } else if matches!(mode, SdrMode::Packet | SdrMode::Auto) {
        classifier.use_external_pagers();
        classifier.set_data_only(mode == SdrMode::Packet);
    }
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
        /// The multipath equalizer ahead of the discriminator, and its output.
        eq: Option<Box<scannerd_engine::cma::Cma>>,
        eq_iq: Vec<num_complex::Complex32>,
        disc: Vec<f32>,
        /// De-emphasis, the 15 kHz audio band and decimation to ~51 kHz for
        /// the mono sum, and the stereo difference from the 38 kHz
        /// subcarrier when a pilot is locked, blended by the measured S/N.
        audio: Box<WfmAudio>,
        /// This block's interleaved L, R, when the decoder delivered stereo;
        /// cleared with `audio_buf` at the top of every block.
        stereo: Vec<f32>,
        /// Whether the last status pushed had a pilot, so a lock or a loss
        /// goes out at once.
        pilot_seen: bool,
        /// RDS from the 57 kHz subcarrier, which the discriminator's full
        /// multiplex still carries.
        rds: Box<RdsDecoder>,
        /// The decoder's generation last pushed in a status, and when and
        /// with what error rate a status last went out.
        rds_seen: u64,
        rds_pushed: (Instant, f32),
    },
    Packet {
        aprs: AprsDecoder,
        /// One decoder per POCSAG rate: a pager channel does not announce
        /// which it uses, and running all three costs less than guessing.
        pocsag: Vec<PocsagDecoder>,
        flex: FlexDecoder,
    },
    Flex {
        flex: FlexDecoder,
    },
    Am {
        demod: scannerd_engine::am::AmDemod,
        audio: Vec<f32>,
        /// WWV: the time code decoder, fed the channel I/Q.
        hf: Option<Box<crate::hfdata::HfDecoder>>,
    },
    Ssb {
        demod: SsbDemod,
        audio: Vec<f32>,
        /// FT8/FT4: the same audio before the speech AGC, so the decoder
        /// measures signals against each other as they arrived, and the
        /// slot decoder it feeds.
        linear: Vec<f32>,
        slots: Option<Box<scannerd_engine::ft8::SlotDecoder>>,
        /// WEFAX, RTTY, NAVTEX, PSK31: the decoder fed the same audio.
        hf: Option<Box<crate::hfdata::HfDecoder>>,
    },
    P25 {
        rx: Box<P25ChannelReceiver>,
    },
    Dmr {
        rx: Box<DmrChannelReceiver>,
    },
    Nxdn {
        rx: Box<NxdnChannelReceiver>,
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
    /// The voice scanner's demodulator placeholder: the scan's receivers
    /// live in the parallel slot pool ([`ScanSlotRig`]) instead, because the
    /// scanner points them at several candidates at once and rebuilds them
    /// per candidate move. Nothing here needs per-channel state.
    Scan,
}

impl Demod {
    /// The HF data decoder this demodulator feeds, if it has one.
    fn hf_mut(&mut self) -> Option<&mut crate::hfdata::HfDecoder> {
        match self {
            Demod::Ssb { hf: Some(hf), .. } | Demod::Am { hf: Some(hf), .. } => Some(hf),
            _ => None,
        }
    }

    /// `fs_chain` is the inspect chain's output rate. The digital voice
    /// receivers ride that chain's output rather than re-extracting their
    /// own channel from the whole span: each one used to carry a second
    /// full-rate NCO + 2047-tap filter pass per block for a channel the
    /// inspect chain had already produced. The scan mode's receivers are
    /// not built here at all — they live in the per-slot rigs of the
    /// parallel pool ([`build_scan_slot`]).
    fn new(mode: SdrMode, fs_chain: f64, channel_hz: f64) -> Self {
        let fs_out = fs_chain;
        match mode {
            SdrMode::P25 => Demod::P25 {
                rx: Box::new(P25ChannelReceiver::new_on_channel(
                    P25Spec {
                        name: "inspect".into(),
                        freq_hz: channel_hz,
                        // Accept whatever NAC is on the air: the operator
                        // pointed the cursor at it, that is the filter.
                        nac: None,
                    },
                    fs_out,
                )),
            },
            SdrMode::Dmr => Demod::Dmr {
                rx: Box::new(DmrChannelReceiver::new_on_channel(
                    DmrSpec {
                        name: "inspect".into(),
                        freq_hz: channel_hz,
                        color_code: None,
                        slot: None,
                    },
                    fs_out,
                )),
            },
            SdrMode::Nxdn => Demod::Nxdn {
                rx: Box::new(NxdnChannelReceiver::new_on_channel(
                    NxdnSpec {
                        name: "inspect".into(),
                        freq_hz: channel_hz,
                        // Acquire NXDN48 and NXDN96 together; a conventional
                        // channel does not announce which it uses.
                        rate: None,
                        ran: None,
                    },
                    fs_out,
                )),
            },
            SdrMode::Auto => Demod::Auto {
                p25: Box::new(P25ChannelReceiver::new_on_channel(
                    P25Spec { name: "auto".into(), freq_hz: channel_hz, nac: None },
                    fs_out,
                )),
                dmr: Box::new(DmrChannelReceiver::new_on_channel(
                    DmrSpec {
                        name: "auto".into(),
                        freq_hz: channel_hz,
                        color_code: None,
                        slot: None,
                    },
                    fs_out,
                )),
                aprs: AprsDecoder::new(fs_out),
                pocsag: [512u32, 1200, 2400]
                    .into_iter()
                    .map(|baud| PocsagDecoder::new(fs_out, baud))
                    .collect(),
                flex: FlexDecoder::new(fs_out),
            },
            SdrMode::Nfm => Demod::Nfm,
            SdrMode::Scan => Demod::Scan,
            SdrMode::Am | SdrMode::Wwv | SdrMode::Drm => Demod::Am {
                demod: {
                    // The audio band follows the filter: half its width, as
                    // AM sends the audio in both sidebands. A filter change
                    // rebuilds the demodulator (see `remix_inspect`).
                    let (lo, hi) = mode.filter_edges();
                    scannerd_engine::am::AmDemod::with_audio_lowpass(fs_out, ((hi - lo) / 2.0).clamp(2_500.0, 10_000.0))
                },
                audio: Vec::new(),
                hf: crate::hfdata::HfDecoder::new(mode, fs_out).map(Box::new),
            },
            SdrMode::Usb | SdrMode::Lsb | SdrMode::Ft8 | SdrMode::Ft4 | SdrMode::Wefax | SdrMode::Rtty | SdrMode::Navtex | SdrMode::Psk31 | SdrMode::Sitor | SdrMode::Dsc | SdrMode::Hfdl | SdrMode::Pactor | SdrMode::Stanag | SdrMode::Ale | SdrMode::Cw | SdrMode::Js8 | SdrMode::Sstv | SdrMode::Wspr | SdrMode::Olivia | SdrMode::Mfsk | SdrMode::Hell => Demod::Ssb {
                demod: {
                    let (lo, hi) = mode.filter_edges();
                    SsbDemod::with_shape(
                        fs_out,
                        if mode == SdrMode::Lsb { Sideband::Lower } else { Sideband::Upper },
                        lo.abs().min(hi.abs()),
                        lo.abs().max(hi.abs()),
                        mode.filter_shape(),
                    )
                },
                audio: Vec::new(),
                linear: Vec::new(),
                slots: mode.ft8_protocol().map(|p| {
                    let mut d = scannerd_engine::ft8::SlotDecoder::new(p, fs_out);
                    // USDR_FT8_DUMP=<dir> keeps every slot's audio, for
                    // decoding a band again offline.
                    d.set_dump(std::env::var_os("USDR_FT8_DUMP").map(Into::into));
                    // Search what the filter passes: a filter change
                    // rebuilds the demodulator, and this with it.
                    let (lo, hi) = mode.filter_edges();
                    d.set_band(lo.abs().min(hi.abs()), lo.abs().max(hi.abs()));
                    Box::new(d)
                }),
                hf: crate::hfdata::HfDecoder::new(mode, fs_out).map(|mut d| {
                    // The multi-signal decoders search what the filter
                    // passes; a filter change rebuilds them with it.
                    let (lo, hi) = mode.filter_edges();
                    d.set_band(lo.abs().min(hi.abs()), lo.abs().max(hi.abs()));
                    Box::new(d)
                }),
            },
            SdrMode::Wfm => {
                let decim = (fs_out / 48_000.0).round().max(1.0) as usize;
                Demod::Wfm {
                    fm: NbfmDemod::with_deviation(fs_out, WFM_DEVIATION_HZ),
                    disc: Vec::new(),
                    audio: Box::new(WfmAudio::new(fs_out, decim, WFM_DEVIATION_HZ, WFM_DEEMPHASIS_TAU)),
                    // The equalizer's taps were laid out for 3.9 µs at 256
                    // kHz; the chain lands within a few percent of that at
                    // every span, which only stretches the taps a little.
                    // USDR_WFM_NO_EQ=1 leaves it out, for comparing.
                    eq: ((200_000.0..=320_000.0).contains(&fs_out) && std::env::var_os("USDR_WFM_NO_EQ").is_none()).then(|| Box::new(scannerd_engine::cma::Cma::new())),
                    eq_iq: Vec::new(),
                    stereo: Vec::new(),
                    pilot_seen: false,
                    rds: Box::new({
                        let mut d = RdsDecoder::new(fs_out);
                        // USDR_RDS_TRACE=1 logs every group, for diagnosing
                        // a station on the air.
                        d.set_trace(std::env::var_os("USDR_RDS_TRACE").is_some());
                        d
                    }),
                    // Never a generation the decoder has, so the first block
                    // on a new channel pushes a status without the old
                    // station's RDS in it.
                    rds_seen: u64::MAX,
                    rds_pushed: (Instant::now(), 0.0),
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
            SdrMode::Flex => Demod::Flex {
                flex: FlexDecoder::new(fs_out),
            },
            SdrMode::Pager | SdrMode::Ais => Demod::Packet {
                // The per-channel decoders are idle here: PAGER decodes from
                // the wide span through the walking bank instead, AIS through
                // its own two-channel receiver.
                aprs: AprsDecoder::new(fs_out),
                pocsag: Vec::new(),
                flex: FlexDecoder::idle(),
            },
        }
    }

    /// Drop per-run decode state after a break in the sample timeline (a
    /// lost-delivery gap or an acquisition epoch boundary). Framing state
    /// must not stitch samples across a hole that never existed on the air;
    /// the decoders restart their lock from the new run instead.
    fn reset_timeline(&mut self, _fs_out: f64) {
        match self {
            Demod::Nfm | Demod::Scan => {}
            Demod::Wfm {
                fm,
                disc,
                audio,
                stereo,
                rds,
                ..
            } => {
                fm.reset();
                disc.clear();
                audio.reset();
                stereo.clear();
                // Same station, new run of samples: keep what it has sent.
                rds.reset_sync();
            }
            Demod::Packet {
                aprs,
                pocsag,
                flex,
            } => {
                aprs.reset();
                for dec in pocsag.iter_mut() {
                    dec.reset();
                }
                flex.reset();
            }
            Demod::Flex { flex } => flex.reset(),
            Demod::Am { demod, audio, .. } => {
                demod.reset();
                audio.clear();
            }
            // The slot decoder keeps its slot: it places audio by the wall
            // clock, so a hole is silence in the slot, not a splice.
            Demod::Ssb { demod, audio, linear, .. } => {
                demod.reset();
                audio.clear();
                linear.clear();
            }
            Demod::P25 { rx } => rx.reset(),
            Demod::Dmr { rx } => rx.reset(),
            Demod::Nxdn { rx } => rx.reset(),
            Demod::Auto {
                p25,
                dmr,
                aprs,
                pocsag,
                flex,
            } => {
                p25.reset();
                dmr.reset();
                aprs.reset();
                for dec in pocsag.iter_mut() {
                    dec.reset();
                }
                flex.reset();
            }
        }
    }

    /// Audio rate this demodulator delivers, given the chain rate feeding it.
    fn audio_rate(&self, fs_out: f64) -> f64 {
        match self {
            Demod::Wfm { audio, .. } => fs_out / audio.decim() as f64,
            Demod::P25 { rx } => rx.audio_rate(),
            Demod::Dmr { rx } => rx.audio_rate(),
            Demod::Nxdn { rx } => rx.audio_rate(),
            Demod::Flex { .. } => 0.0,
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
    /// Complex channel samples, interleaved I/Q as signed 16-bit values.
    pub iq_rate_hz: u32,
    pub iq_samples: usize,
    /// Peak absolute sample, so a silent recording is obvious without playing it.
    pub peak: f32,
    pub encrypted: bool,
    pub decrypted: bool,
    /// Strongest spectrum-derived SNR seen during the call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_snr_db: Option<f32>,
    /// Average carrier offset reported by the protocol demodulator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freq_error_hz: Option<f32>,
    /// Fraction of captured audio admitted by the voice/noise gate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voiced_fraction: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub algorithm: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_id: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tone: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mdc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digital_protocol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_code: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manufacturer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_options: Option<u8>,
    pub emergency: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub talker_alias: Option<String>,
    /// Smoothed voice-frame bit-error metric, 0-100, where the protocol
    /// reports one. A call that plays scrambled usually reads high — weak
    /// signal, not the wrong decoder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_pct: Option<u8>,
    #[serde(skip)]
    pub audio: Vec<i16>,
    #[serde(skip)]
    pub iq: Vec<i16>,
}

/// How many finished calls to keep. A handful is enough to look back over what
/// just happened without letting a busy channel grow without bound.
const RECENT_CALLS: usize = 16;

/// Voice audio accumulating for one call. Shared by the P25/DMR arm and the
/// scanner, which also records analog calls on squelch edges — the ring
/// buffer, the id stamping and the i16 scaling are one mechanism, not two.
struct CallRecorder {
    active: bool,
    started_ms: u64,
    audio_cap_samples: usize,
    iq_cap_samples: usize,
    iq_rate_hz: u32,
    buf: Vec<i16>,
    /// Interleaved signed 16-bit little-endian I/Q on export.
    iq: Vec<i16>,
}

/// Hard ceiling on one recorded call. A continuous carrier — a weather
/// broadcast, a control channel — holds squelch open indefinitely, and
/// without a cap that "call" grows at ~100 KB/s of i16 for as long as it
/// holds. Three minutes covers all but the longest real conversations; the
/// audio and channel I/Q are truncated, not the call dropped.
const MAX_CALL_SECS: f32 = 180.0;

impl CallRecorder {
    fn new() -> Self {
        Self {
            active: false,
            started_ms: 0,
            audio_cap_samples: usize::MAX,
            iq_cap_samples: usize::MAX,
            iq_rate_hz: 0,
            buf: Vec::new(),
            iq: Vec::new(),
        }
    }

    fn start(&mut self, audio_rate_hz: f64, iq_rate_hz: f64, now_ms: u64) {
        self.active = true;
        self.started_ms = now_ms;
        self.audio_cap_samples =
            (audio_rate_hz.max(1.0) as usize).max(1) * MAX_CALL_SECS as usize;
        self.iq_rate_hz = iq_rate_hz.round().max(1.0) as u32;
        self.iq_cap_samples = self.iq_rate_hz as usize * MAX_CALL_SECS as usize;
        self.buf.clear();
        self.iq.clear();
    }

    fn push(&mut self, audio: &[f32]) {
        if self.buf.len() >= self.audio_cap_samples {
            return;
        }
        let room = self.audio_cap_samples - self.buf.len();
        self.buf.extend(
            audio
                .iter()
                .take(room)
                .map(|&x| (x.clamp(-1.0, 1.0) * 28_000.0) as i16),
        );
    }

    fn push_iq(&mut self, iq: &[num_complex::Complex32]) {
        let have = self.iq.len() / 2;
        if have >= self.iq_cap_samples {
            return;
        }
        let room = self.iq_cap_samples - have;
        self.iq.extend(iq.iter().take(room).flat_map(|x| {
            [
                (x.re.clamp(-1.0, 1.0) * 32_000.0) as i16,
                (x.im.clamp(-1.0, 1.0) * 32_000.0) as i16,
            ]
        }));
    }

    fn stop(&mut self) -> (Vec<i16>, Vec<i16>, u32, u64) {
        self.active = false;
        (
            std::mem::take(&mut self.buf),
            std::mem::take(&mut self.iq),
            self.iq_rate_hz,
            self.started_ms,
        )
    }
}

/// File a finished call into the Last Heard ring, dropping the oldest when
/// the ring is full. Sub-second fragments are not filed at all: a sync blip
/// that ended before it said anything has no replay value and no story.
fn file_call(calls: &Mutex<VecDeque<RecordedCall>>, record: RecordedCall) {
    if record.duration_s < 0.4 {
        return;
    }
    let mut ring = calls.lock().expect("calls");
    if ring.len() >= RECENT_CALLS {
        ring.pop_front();
    }
    ring.push_back(record);
}

/// File a recorder that was still running when its call was cut off without
/// a close event — slot released or reseated mid-call, or the operator
/// leaving SCAN: keep what it caught, drop silence.
fn flush_scan_rec(
    rec: &mut CallRecorder,
    meta: Option<(ScanMode, u32)>,
    freq: f64,
    fallback_rate: u32,
    calls: &Mutex<VecDeque<RecordedCall>>,
    next_id: &mut u64,
) {
    if !rec.active {
        return;
    }
    let (buf, iq, iq_rate_hz, started) = rec.stop();
    let (mode, rate) = meta.unwrap_or((ScanMode::Nfm, fallback_rate));
    let rate = rate.max(1);
    let peak = call_peak(&buf);
    let duration = buf.len() as f32 / rate as f32;
    if peak >= 0.002 && duration >= 0.4 {
        file_call(
            calls,
            RecordedCall {
                id: *next_id,
                protocol: mode.protocol().into(),
                started_ms: started,
                duration_s: duration,
                freq_hz: freq,
                rate_hz: rate,
                samples: buf.len(),
                iq_rate_hz,
                iq_samples: iq.len() / 2,
                peak,
                encrypted: false,
                decrypted: false,
                peak_snr_db: None,
                freq_error_hz: None,
                voiced_fraction: None,
                algorithm: None,
                key_id: None,
                tone: None,
                mdc: None,
                digital_protocol: None,
                color_code: None,
                slot: None,
                source_id: None,
                target_id: None,
                group: None,
                manufacturer: None,
                service_options: None,
                emergency: false,
                talker_alias: None,
                error_pct: None,
                audio: buf,
                iq,
            },
        );
        *next_id += 1;
    }
}

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
/// A trunking grant rendered as one human-readable line.
fn grant_repr(grant: &scannerd_engine::p25::conventional::TrunkGrant) -> String {
    format!(
        "grant: TG {} on {:.4} MHz{}",
        grant.talkgroup,
        grant.freq_hz / 1e6,
        if grant.group { "" } else { " (indiv)" }
    )
}

/// NXDN does not announce a band plan in-band, so a control-channel
/// assignment names a carrier *number*; consecutive grants on the same
/// number are directly comparable, but it cannot be turned into Hz here.
fn nxdn_grant_repr(grant: &scannerd_engine::nxdn::NxdnGrant) -> String {
    format!(
        "assignment: {} {} → channel {}",
        if grant.group { "TG" } else { "ID" },
        grant.talkgroup,
        grant.channel_number
    )
}

/// Wrap a control-channel observation in the decode-event pipeline.
fn grant_event(protocol: &str, repr: &str) -> DecodeEvent {
    DecodeEvent {
        protocol: protocol.into(),
        kind: "trunking".into(),
        summary: repr.into(),
        valid: true,
        fields: std::collections::BTreeMap::new(),
    }
}

fn packet_event(packet: &scannerd_engine::aprs::AprsPacket) -> DecodeEvent {
    let mut fields = std::collections::BTreeMap::new();
    fields.insert("from".into(), packet.from.clone());
    fields.insert("to".into(), packet.to.clone());
    if !packet.path.is_empty() {
        fields.insert("via".into(), packet.path.join(","));
    }
    if let (Some(lat), Some(lon)) = (packet.lat, packet.lon) {
        fields.insert("lat".into(), format!("{lat:.5}"));
        fields.insert("lon".into(), format!("{lon:.5}"));
    }
    // The path is the story of the packet: who heard it and where it died.
    let via = if packet.path.is_empty() {
        String::new()
    } else {
        format!(" via {}", packet.path.join(","))
    };
    DecodeEvent {
        protocol: "APRS".into(),
        kind: "AX.25 UI frame".into(),
        summary: format!("{} > {}{} · {}", packet.from, packet.to, via, packet.text),
        // Only frames whose FCS checked out are handed back by the decoder.
        valid: true,
        fields,
    }
}

/// An AIS message as a decode: what it says in a line, and every field it
/// carried, positions as decimal degrees in `lat` and `lon` for the map.
fn ais_event(f: &scannerd_engine::ais::AisFrame) -> DecodeEvent {
    use scannerd_engine::ais::{kind_name, ship_type_name, status_name};
    let m = &f.msg;
    let mut fields = std::collections::BTreeMap::new();
    let mut put = |k: &str, v: String| {
        fields.insert(k.to_string(), v);
    };
    let mmsi = format!("{:09}", m.mmsi);
    put("mmsi", mmsi.clone());
    put("from", mmsi.clone());
    put("channel", format!("{} {}", f.channel, if f.channel == 'A' { "161.975" } else { "162.025" }));
    put("type", m.kind.to_string());
    if let (Some(lat), Some(lon)) = (m.lat, m.lon) {
        put("lat", format!("{lat:.5}"));
        put("lon", format!("{lon:.5}"));
    }
    if let Some(v) = m.sog_kn {
        put("sog", format!("{v:.1}"));
    }
    if let Some(v) = m.cog {
        put("cog", format!("{v:.1}"));
    }
    if let Some(v) = m.heading {
        put("heading", v.to_string());
    }
    if let Some(v) = m.status.map(status_name).filter(|s| !s.is_empty()) {
        put("status", v.to_string());
    }
    if let Some(v) = &m.name {
        put("name", v.clone());
    }
    if let Some(v) = &m.callsign {
        put("callsign", v.clone());
    }
    if let Some(t) = m.ship_type {
        put("shipType", t.to_string());
        if !ship_type_name(t).is_empty() {
            put("shipKind", ship_type_name(t).to_string());
        }
    }
    if let Some(v) = &m.destination {
        put("destination", v.clone());
    }
    if let Some(v) = m.imo {
        put("imo", v.to_string());
    }
    if let Some(v) = &m.eta {
        put("eta", v.clone());
    }
    if let Some(v) = m.draught_m {
        put("draught", format!("{v:.1}"));
    }
    if let Some((bow, stern, port, stbd)) = m.dims
        && bow + stern > 0
    {
        put("length", (bow + stern).to_string());
        put("beam", (u16::from(port) + u16::from(stbd)).to_string());
    }
    if let Some(v) = m.altitude_m {
        put("altitude", v.to_string());
    }
    if let Some(v) = &m.utc {
        put("utc", v.clone());
    }
    let who = m.name.clone().unwrap_or_else(|| mmsi.clone());
    let mut parts = vec![who];
    match m.kind {
        5 | 24 => {
            if let Some(c) = &m.callsign {
                parts.push(c.clone());
            }
            if let Some(t) = m.ship_type.map(ship_type_name).filter(|t| !t.is_empty()) {
                parts.push(t.to_string());
            }
            if let Some(d) = &m.destination {
                parts.push(format!("to {d}"));
            }
        }
        4 => parts.extend(m.utc.clone()),
        _ => {
            if let Some(v) = m.sog_kn {
                parts.push(format!("{v:.1} kn"));
            }
            if let Some(v) = m.cog {
                parts.push(format!("{v:.0}°"));
            }
            if let Some(v) = m.status.map(status_name).filter(|s| !s.is_empty()) {
                parts.push(v.to_string());
            }
        }
    }
    DecodeEvent {
        protocol: "AIS".into(),
        kind: kind_name(m.kind).into(),
        summary: parts.join(" · "),
        // Only frames whose FCS checked out are handed back.
        valid: true,
        fields,
    }
}

/// The stereo decoder's reading with the multipath equalizer's beside it.
fn with_equalizer(mut st: StereoStatus, eq: Option<&scannerd_engine::cma::Cma>) -> StereoStatus {
    if let Some(eq) = eq {
        let (r_in, _) = eq.ripple_now();
        st.multipath_pct = Some((r_in * 1000.0).round() / 10.0);
        st.equalizer = eq.engaged();
    }
    st
}

/// An FT8/FT4 decode as the log carries it: the message as the operator
/// reads it, and its measurements and parts as fields.
fn ft8_event(slot_ms: u64, d: &scannerd_engine::ft8::Decode) -> DecodeEvent {
    let secs = slot_ms / 1000;
    let mut fields = std::collections::BTreeMap::new();
    fields.insert("snr".to_string(), format!("{:.0}", d.snr_db));
    fields.insert("dt".to_string(), format!("{:.1}", d.dt_s));
    fields.insert("df".to_string(), format!("{:.0}", d.freq_hz));
    fields.insert("slot".to_string(), format!("{:02}{:02}{:02}", (secs / 3600) % 24, (secs / 60) % 60, secs % 60));
    fields.insert("slotMs".to_string(), slot_ms.to_string());
    fields.insert("type".to_string(), d.message.kind.to_string());
    fields.insert("cq".to_string(), if d.message.is_cq { "yes" } else { "no" }.to_string());
    fields.insert("ldpcErrors".to_string(), d.ldpc_errors.to_string());
    for (k, v) in [("to", &d.message.to), ("de", &d.message.de), ("extra", &d.message.extra), ("grid", &d.message.grid)] {
        if let Some(v) = v {
            fields.insert(k.to_string(), v.clone());
        }
    }
    DecodeEvent {
        protocol: d.protocol.label().into(),
        kind: if d.message.is_cq { "CQ".into() } else { d.message.kind.into() },
        summary: d.text.clone(),
        valid: true,
        fields,
    }
}

fn pocsag_event(msg: &scannerd_engine::PocsagMessage) -> DecodeEvent {
    let mut fields = std::collections::BTreeMap::new();
    fields.insert("capcode".into(), msg.capcode.to_string());
    fields.insert("function".into(), msg.function.to_string());
    fields.insert("baud".into(), msg.baud.to_string());
    fields.insert(
        "format".into(),
        match msg.format {
            scannerd_engine::PocsagFormat::ToneOnly => "tone only".into(),
            scannerd_engine::PocsagFormat::Alphanumeric => "alphanumeric".into(),
            scannerd_engine::PocsagFormat::Numeric => "numeric".into(),
        },
    );
    // The decoder's own presentation, plus both raw interpretations —
    // alphanumeric and BCD-numeric readings of the same words can disagree,
    // and the log is where an operator tells them apart.
    if !msg.text.is_empty() {
        fields.insert("message".into(), msg.text.clone());
    }
    if !msg.alpha_text.is_empty() && msg.alpha_text != msg.text {
        fields.insert("alphaText".into(), msg.alpha_text.clone());
    }
    if !msg.numeric_text.is_empty() && msg.numeric_text != msg.text {
        fields.insert("numericText".into(), msg.numeric_text.clone());
    }
    // Structured parse (weather alerts, Skyper IDs, and the like) when the
    // decoder recognised a pattern.
    if let Some(parsed) = &msg.parsed {
        fields.insert("parsed".into(), parsed.clone());
    }
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
        summary: match (&msg.parsed, msg.text.is_empty()) {
            (Some(p), _) => format!("{} · {}", msg.capcode, p),
            (_, true) => format!("{} · tone only", msg.capcode),
            (_, false) => format!("{} · {}", msg.capcode, msg.text),
        },
        valid: true,
        fields,
    }
}

fn flex_event(msg: &scannerd_engine::FlexMessage) -> DecodeEvent {
    use scannerd_engine::{FlexFormat, FlexFragment};
    let mut fields = std::collections::BTreeMap::new();
    fields.insert("capcode".into(), msg.capcode.to_string());
    fields.insert("cycle".into(), msg.cycle.to_string());
    fields.insert("frame".into(), msg.frame.to_string());
    fields.insert("phase".into(), msg.phase.to_string());
    fields.insert(
        "format".into(),
        match msg.format {
            FlexFormat::Secure => "secure".into(),
            FlexFormat::ShortInstruction => "short instruction".into(),
            FlexFormat::ShortMessage => "short message / tone".into(),
            FlexFormat::StandardNumeric => "standard numeric".into(),
            FlexFormat::SpecialNumeric => "special numeric".into(),
            FlexFormat::Alphanumeric => "alphanumeric".into(),
            FlexFormat::Binary => "binary".into(),
            FlexFormat::NumberedNumeric => "numbered numeric".into(),
        },
    );
    fields.insert("baud".into(), msg.baud.to_string());
    fields.insert("symbolRate".into(), msg.symbol_rate.to_string());
    fields.insert("levels".into(), msg.levels.to_string());
    fields.insert(
        "addressType".into(),
        if msg.long_address {
            format!("{} (long)", msg.address_type)
        } else {
            msg.address_type.clone()
        },
    );
    // Fragmented messages arrive in pieces across frames; without this a
    // truncated-looking page looks like a decode fault.
    let frag = match msg.fragment {
        FlexFragment::Complete => None,
        FlexFragment::First => Some(format!(
            "part 1 · {}{}",
            msg.fragment_number.map(|n| n.to_string()).unwrap_or_else(|| "?".into()),
            if msg.complete { "" } else { ", more to come" }
        )),
        _ => Some(format!(
            "{} part{} of {}",
            match msg.fragment {
                FlexFragment::Middle => "middle",
                FlexFragment::Continuation => "continuation",
                _ => "later",
            },
            msg.fragment_number.map(|n| format!(" {}", n)).unwrap_or_default(),
            msg.message_number.map(|n| n.to_string()).unwrap_or_else(|| "?".into()),
        )),
    };
    if let Some(frag) = frag {
        fields.insert("fragment".into(), frag);
    }
    if msg.reassembled {
        fields.insert("reassembled".into(), "true".into());
    }
    if msg.priority {
        fields.insert("priority".into(), "true".into());
    }
    if msg.maildrop == Some(true) {
        fields.insert("maildrop".into(), "true".into());
    }
    if let Some(retrieval) = msg.retrieval {
        fields.insert("messageRetrieval".into(), retrieval.to_string());
    }
    if let Some(sub) = &msg.secure_subtype {
        fields.insert("secureSubtype".into(), sub.clone());
    }
    if !msg.text.is_empty() {
        fields.insert("message".into(), msg.text.clone());
    }
    // Structured parse when the decoder recognised a pattern in the text.
    if let Some(parsed) = &msg.parsed {
        fields.insert("parsed".into(), parsed.clone());
    }
    fields.insert("fecCorrected".into(), msg.fec_corrected.to_string());
    fields.insert("fecUncorrectable".into(), msg.fec_uncorrectable.to_string());
    if let Some(ok) = msg.payload_checksum_ok {
        fields.insert("payloadChecksumOk".into(), ok.to_string());
    }
    // 21-bit words, again shown as words rather than bytes.
    if !msg.raw_words.is_empty() {
        fields.insert("rawWords".into(), hex_words(&msg.raw_words));
    }
    let checksum_failed = msg.payload_checksum_ok == Some(false);
    DecodeEvent {
        protocol: "FLEX".into(),
        kind: "page".into(),
        summary: match (&msg.parsed, msg.text.is_empty()) {
            (Some(p), _) if checksum_failed => {
                format!("{} · {} · checksum failed", msg.capcode, p)
            }
            (Some(p), _) => format!("{} · {}", msg.capcode, p),
            (_, true) if msg.format == FlexFormat::ShortMessage => {
                format!("{} · tone", msg.capcode)
            }
            (_, true) => format!("{} · no text", msg.capcode),
            (_, false) if checksum_failed => {
                format!("{} · {} · checksum failed", msg.capcode, msg.text)
            }
            (_, false) => format!("{} · {}", msg.capcode, msg.text),
        },
        valid: !checksum_failed && msg.fec_uncorrectable == 0,
        fields,
    }
}

/// A digital voice call starting or ending, as a decode-log line.
fn call_event(mode: SdrMode, ev: &CallEvent) -> DecodeEvent {
    let protocol = match mode {
        SdrMode::P25 => "P25",
        SdrMode::Nxdn => "NXDN",
        _ => "DMR",
    };
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
    sorted_scratch: &mut Vec<f32>,
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
    // a signal-to-noise ratio rather than a bandwidth ratio. The caller shares
    // one sorted copy across every per-frame measurement that needs it; the
    // first to run this frame pays for the sort.
    if sorted_scratch.len() != n {
        sorted_scratch.clear();
        sorted_scratch.extend_from_slice(shown);
        sorted_scratch.sort_unstable_by(f32::total_cmp);
    }
    let floor_lin = 10f64.powf(f64::from(sorted_scratch[n / 2]) / 10.0) * width;

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
/// energy actually sits, which is what a frequency calibration needs — and
/// where the scanner wants to land on a candidate only known to bin
/// resolution.
pub(crate) fn carrier_offset_hz(
    shown: &[f32],
    shown_rate: f64,
    channel_offset_hz: f64,
    bandwidth_hz: f64,
    sorted_scratch: &mut Vec<f32>,
) -> Option<f64> {
    let n = shown.len();
    if n < 16 {
        return None;
    }
    // The caller reuses one sorted copy across the per-frame measurements;
    // this runs before channel_level, so it fills the scratch first.
    if sorted_scratch.len() != n {
        sorted_scratch.clear();
        sorted_scratch.extend_from_slice(shown);
        sorted_scratch.sort_unstable_by(f32::total_cmp);
    }
    let floor = sorted_scratch[n * 3 / 10];

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
///
/// Also the candidate finder the voice scanner sweeps with — it looks for the
/// same thing (a carrier standing over the local noise floor) from the full
/// span rather than the display window.
pub(crate) fn find_peaks(
    smoothed: &[f32],
    center_hz: f64,
    rate_hz: f64,
    spur_offsets_hz: &[f64],
    floor: &[f32],
    history: &DecodeHistory,
) -> Vec<PeakMarker> {
    let mut peaks = Vec::new();
    let n = smoothed.len();
    if n < 16 || floor.len() != n {
        return peaks;
    }
    // The per-bin floor is temporally smoothed, so both the reference and the
    // bin-to-bin spread measured against it are stable. The threshold adapts
    // to how noisy the band actually is instead of a fixed +8 dB that either
    // chases noise spikes on a quiet band or misses marginal carriers on a
    // busy one.
    let diffs: Vec<f32> = smoothed
        .iter()
        .zip(floor)
        .map(|(&s, &f)| (s - f).max(0.0))
        .collect();
    let mean_diff = diffs.iter().sum::<f32>() / n as f32;
    let sigma = (diffs
        .iter()
        .map(|d| {
            let e = *d - mean_diff;
            e * e
        })
        .sum::<f32>()
        / n as f32)
        .sqrt();
    let threshold_db = (mean_diff + 3.0 * sigma).clamp(6.0, 20.0);

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
        if p > floor[i] + threshold_db
            && p > smoothed[i - 1]
            && p >= smoothed[i - 2]
            && p > smoothed[i + 1]
            && p >= smoothed[i + 2]
        {
            // The same axis the offsets above and every viewer use: bin 0
            // at the low edge, the last bin at the high edge.
            let freq_hz = center_hz - rate_hz * 0.5 + i as f64 * rate_hz / (n - 1) as f64;
            let snr_db = p - floor[i];
            peaks.push(PeakMarker {
                freq_hz,
                pwr_db: p,
                snr_db,
                label: history.label_for(freq_hz),
            });
        }
    }
    // Strongest first, but the dedupe window only suppresses a peak when a
    // STRONGER neighbour is genuinely adjacent — weak signals in the same
    // window used to be culled outright by strongest-wins.
    peaks.sort_by(|a, b| b.pwr_db.total_cmp(&a.pwr_db));
    let mut filtered: Vec<PeakMarker> = Vec::new();
    for p in peaks {
        let dup = filtered.iter().any(|q| {
            (q.freq_hz - p.freq_hz).abs() < 18_000.0
                && q.snr_db > p.snr_db + 10.0
        });
        if !dup {
            filtered.push(p);
            if filtered.len() >= 12 {
                break;
            }
        }
    }
    filtered.sort_by(|a, b| a.freq_hz.total_cmp(&b.freq_hz));
    filtered
}

/// One parallel test channel of the voice scanner: a narrowband chain mixed
/// to the slot's candidate, plus whatever receivers the scan modes want on
/// it. Every receiver — analog and digital alike — reads the chain's
/// already-extracted channel, so a slot costs one mix-and-filter pass per
/// block no matter how many decoders it carries, and several slots cost no
/// extra tuner writes and no extra FFTs.
///
/// Built by [`crate::scan::Action::Inspect`], freed by
/// [`crate::scan::Action::Release`]. The audio-rate filters mirror the shared
/// monitor chain (`mon_gate` and friends), because the scanner's voice
/// threshold (`VOICE_RMS`) is calibrated against exactly that construction —
/// and like the shared chain they must be rated for the chain's real output
/// rate, not a nominal one: every timer in them is sample-counted.
struct ScanSlotRig {
    freq_hz: f64,
    chain: DecodeChain,
    chain_iq: Vec<num_complex::Complex32>,
    p25: Option<Box<P25ChannelReceiver>>,
    dmr: Option<Box<DmrChannelReceiver>>,
    am: Option<scannerd_engine::am::AmDemod>,
    am_audio: Vec<f32>,
    nbfm: Option<NbfmDemod>,
    fm_out: Demodulated,
    mon_notch: AutoNotch,
    mon_gate: NoiseGate,
    mon_leveler: Leveler,
    /// Gated, levelled NFM voice of the last block: the scanner's voice
    /// evidence reads its RMS, and a lock on this slot routes it to LISTEN.
    nfm_voice: Vec<f32>,
    /// Channel SNR at `freq_hz`, refreshed per display frame by the worker
    /// and fed to the receivers (for their summaries) and the squelch.
    snr_db: f32,
}

/// Build a slot rig mixed to `freq_hz` inside the span at `span_rate_hz`.
fn build_scan_slot(
    freq_hz: f64,
    span_rate_hz: f64,
    span_center_hz: f64,
    scan_modes: &[crate::scan::ScanMode],
) -> ScanSlotRig {
    let mut chain = DecodeChain::new(span_rate_hz, INSPECT_BANDWIDTH_HZ, INSPECT_RATE);
    // The span IQ sits at the hardware LO; the same offset math the shared
    // inspect chain uses puts the candidate at DC.
    chain.set_offset(freq_hz - span_center_hz);
    let fs_out = chain.fs_out();
    ScanSlotRig {
        freq_hz,
        // The digital receivers ride the chain's output (`chain_iq`) rather
        // than extracting their own channel from the span: one shared
        // mix-and-filter pass per slot per block instead of one per receiver,
        // at an identical output-rate grid (see `new_on_channel`).
        p25: scan_modes.contains(&crate::scan::ScanMode::P25).then(|| {
            Box::new(P25ChannelReceiver::new_on_channel(
                P25Spec { name: "scan".into(), freq_hz, nac: None },
                fs_out,
            ))
        }),
        dmr: scan_modes.contains(&crate::scan::ScanMode::Dmr).then(|| {
            Box::new(DmrChannelReceiver::new_on_channel(
                DmrSpec {
                    name: "scan".into(),
                    freq_hz,
                    color_code: None,
                    slot: None,
                },
                fs_out,
            ))
        }),
        am: scan_modes
            .contains(&crate::scan::ScanMode::Am)
            .then(|| scannerd_engine::am::AmDemod::new(fs_out)),
        nbfm: scan_modes
            .contains(&crate::scan::ScanMode::Nfm)
            .then(|| NbfmDemod::new(fs_out)),
        chain,
        chain_iq: Vec::new(),
        am_audio: Vec::new(),
        fm_out: Demodulated::default(),
        // Rated for the audio these actually receive: the slot chain's real
        // output rate (span / round(span/48 kHz) — 47 627.9 Hz at a 2.048 MS/s
        // span, never exactly 48 kHz). Built for a nominal 8 kHz, the gate's
        // close delay and ambiguity latch ran ~6× short and chopped marginal
        // carriers into fragments.
        mon_notch: AutoNotch::new(fs_out),
        mon_gate: NoiseGate::new(fs_out),
        mon_leveler: Leveler::new(fs_out),
        nfm_voice: Vec::new(),
        snr_db: 0.0,
    }
}

/// Re-mix every live slot for a new span centre: the chains' NCO offsets are
/// relative to the LO, so a span move invalidates them all at once.
/// A scan span move shifts the LO under every live slot. Nothing about a
/// slot depends on where the span sits — the receivers ride their own
/// chain's already-extracted channel — so re-pointing each chain at the new
/// LO is the whole job: the rigs keep their filters, FFT plans and decoder
/// state instead of being rebuilt per window hop.
fn retune_scan_slots(slots: &mut [Option<ScanSlotRig>], span_center_hz: f64) {
    for slot in slots.iter_mut().flatten() {
        slot.chain.set_offset(slot.freq_hz - span_center_hz);
    }
}

/// The worker locals a re-mix of the narrowband inspect chain touches.
/// Bundled so the shared helper can borrow them without a fifteen-parameter
/// signature; constructed fresh at each call site.
struct InspectParts<'a> {
    inspect_chain: &'a mut DecodeChain,
    classifier: &'a mut SignalClassifier,
    demod: &'a mut Demod,
    scope_fm: &'a mut NbfmDemod,
    capture: &'a Mutex<CaptureBuffer>,
    afc: &'a mut scannerd_engine::Afc,
    afc_last_reported: &'a mut f32,
}

/// Rebuild the inspect chain, its classifier and the demodulator for a new
/// channel or mode. The Tune/Inspect/Rate/Mode commands and the scanner's
/// candidate moves all want exactly this sequence: a fresh chain mixed to the
/// new offset, a demodulator built against it, capture cleared, AFC re-based.
/// Rebuilding the scope's FM demodulator too is deliberate — its filter state
/// belongs to the channel that just ended.
fn remix_inspect(
    p: &mut InspectParts<'_>,
    rate_hz: f64,
    span_center_hz: f64,
    inspect_hz: f64,
    lo_off: f64,
    mode: SdrMode,
) {
    let (chain, classifier) =
        build_inspect(rate_hz, inspect_hz - span_center_hz - lo_off, mode);
    *p.inspect_chain = chain;
    *p.classifier = classifier;
    *p.demod = Demod::new(mode, p.inspect_chain.fs_out(), inspect_hz);
    *p.scope_fm = NbfmDemod::with_deviation(p.inspect_chain.fs_out(), 3_000.0);
    p.capture
        .lock()
        .expect("SDR capture")
        .clear(inspect_hz, p.inspect_chain.fs_out());
    p.afc.set_base(0.0);
    *p.afc_last_reported = 0.0;
}

/// Root-mean-square of a block of audio — the quick "is anything here"
/// measure behind the scanner's analog voice evidence.
fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sumsq: f64 = samples.iter().map(|&x| f64::from(x) * f64::from(x)).sum();
    (sumsq / samples.len() as f64).sqrt() as f32
}

/// Peak absolute sample of a finished recording, as its fraction of full
/// scale, so a silent one is obvious without playing it.
///
/// `CallRecorder` stores `x.clamp(-1, 1) * 28_000` (headroom below i16 full
/// scale), so the reference is 28 000. Dividing by 32 768 understated every
/// peak by 1.35 dB and made the silence gate (`peak >= 0.002` in
/// `flush_scan_rec`) trip 2.4× earlier than intended.
fn call_peak(buf: &[i16]) -> f32 {
    buf.iter()
        .map(|&v| (v as f32 / 28_000.0).abs())
        .fold(0.0f32, f32::max)
}

/// Run one scanner action against the worker state. Shared by the scan
/// control command and the two places the scanner is stepped. `TuneSpan`
/// carries the same inspect-follow rule and status bookkeeping as the Tune
/// command, because it *is* a tune — just one the scanner asked for.
#[allow(clippy::too_many_arguments)]
fn apply_scan_action(
    action: crate::scan::Action,
    inspect: &mut InspectParts<'_>,
    scan_slots: &mut Vec<Option<ScanSlotRig>>,
    events: &broadcast::Sender<SdrEvent>,
    status: &Mutex<SdrStatus>,
    current_freq: &mut f64,
    tuned_freq: f64,
    inspect_hz: &mut f64,
    pending_tune: &mut Option<f64>,
    rate_hz: f64,
    lo_offset: bool,
    mode: SdrMode,
    scan_modes: &[crate::scan::ScanMode],
) {
    match action {
        crate::scan::Action::Inspect { slot, freq_hz } => {
            // Inside the current span by construction: re-mix, no tuner write.
            let clamped = freq_hz.clamp(
                *current_freq - display_rate_for(rate_hz, lo_offset, mode) * 0.49,
                *current_freq + display_rate_for(rate_hz, lo_offset, mode) * 0.49,
            );
            if scan_slots.len() <= slot {
                scan_slots.resize_with(slot + 1, || None);
            }
            // The span centre the receivers and the chain NCO share is the
            // hardware LO, not the display centre.
            let span_center = *current_freq + lo_offset_for(rate_hz, lo_offset, mode);
            scan_slots[slot] = Some(build_scan_slot(clamped, rate_hz, span_center, scan_modes));
        }
        crate::scan::Action::Release { slot } => {
            if slot < scan_slots.len() {
                scan_slots[slot] = None;
            }
        }
        crate::scan::Action::TuneSpan(f) => {
            if (*current_freq - f).abs() >= 1.0 {
                let prev = *current_freq;
                *current_freq = f;
                let shown = display_rate_for(rate_hz, lo_offset, mode);
                if (*inspect_hz - prev).abs() < 1.0
                    || *inspect_hz < f - shown * 0.49
                    || *inspect_hz > f + shown * 0.49
                {
                    *inspect_hz = f;
                }
                *pending_tune = Some(f);
                remix_inspect(
                    inspect,
                    rate_hz,
                    *current_freq,
                    *inspect_hz,
                    lo_offset_for(rate_hz, lo_offset, mode),
                    mode,
                );
                // Every live slot was mixed against the old span centre;
                // re-point the chains and the receivers ride along.
                let span_center = f + lo_offset_for(rate_hz, lo_offset, mode);
                retune_scan_slots(scan_slots, span_center);
                // Deliberately not stamping freq_hz: it reports where the
                // tuner is, and it is not there yet — the confirmed value
                // arrives with Event::State.
                if let Ok(mut s) = status.lock() {
                    s.min_freq_hz = tuned_freq - shown / 2.0;
                    s.max_freq_hz = tuned_freq + shown / 2.0;
                    s.inspect_hz = *inspect_hz;
                    s.audio_rate_hz = inspect.demod.audio_rate(inspect.inspect_chain.fs_out());
                    let _ = events.send(SdrEvent::Status(s.clone()));
                }
            }
        }
        crate::scan::Action::Notice(m) => {
            let _ = events.send(SdrEvent::Decode {
                inspect_hz: *inspect_hz,
                event: DecodeEvent {
                    protocol: "SCAN".into(),
                    kind: "scan".into(),
                    summary: m,
                    valid: true,
                    fields: std::collections::BTreeMap::new(),
                },
            });
        }
    }
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
    let mut zoom_view = ZoomView::new(cfg.freq_hz);
    // The center of the window the last block showed, which a hold that
    // names no center keeps.
    let mut shown_center_hz = cfg.freq_hz;
    // Bins at 1x zoom. The browser asks for about one bin per device pixel
    // of its spectrum (see SdrCmd::DisplayBins); 1024 until it does.
    let mut display_bins = ZOOM_TARGET_BINS;
    // What the operator asked for versus what the tuner confirmed. A retune
    // can fail — this dongle stalls its I2C bus often enough that it will —
    // and everything that labels or extracts a frequency has to follow the
    // second one. Believing the request is how the display ends up insisting
    // a station is at 99.1 while the radio is still sitting on 100.
    let mut tuned_freq;
    let mut lo_offset_chosen = cfg.lo_offset.is_some();
    let mut lo_offset = cfg.lo_offset.unwrap_or(false);
    let mut clip_guard_on = cfg.clip_guard;
    let mut tuner_agc_on = cfg.tuner_agc;
    let mut processing = cfg.processing;
    let mut inspect_hz = current_freq;
    let remotes = cfg.remotes.clone();
    let mut notches = Notches {
        rf: cfg.rf_notch,
        dab: cfg.dab_notch,
    };

    // Keep trying rather than dying on the spot. The dongle is routinely busy
    // for a moment after a previous run exits, and a server that gives up
    // there stays dead until someone notices and restarts it — with no radio,
    // the recovery loop below never gets to run at all.
    let (dev, serial, tuner, host) = loop {
        match open_sdr(
            &remotes,
            current_serial.as_deref(),
            current_freq,
            current_rate,
            current_gain,
            LoRequest { on: &mut lo_offset, chosen: lo_offset_chosen, mode: current_mode },
            notches,
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
    // The driver is free to clamp the requested rate (device.rs keeps the
    // value it actually selected and logs the substitution). Every chain,
    // classifier and demodulator below must be built for the rate samples
    // really arrive at, and the status must publish it — believing the
    // request makes block_ms and every inspect-path filter lie by the same
    // ratio, and a classifier told the wrong rate misses syncs outright.
    current_rate = dev.rate;

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
            gain_now_db: None,
            ppm,
            min_freq_hz: current_freq - display_rate_for(current_rate, lo_offset, current_mode) / 2.0,
            max_freq_hz: current_freq + display_rate_for(current_rate, lo_offset, current_mode) / 2.0,
            fft_size,
            peak_iq: 0.0,
            inspect_hz,
            inspect_rate_hz: 0.0,
            dropped_blocks: 0,
            lagged_blocks: 0,
            lost_samples: 0,
            gap_events: 0,
            error: None,
            mode: current_mode,
            audio_rate_hz: 0.0,
            lo_offset,
            lo_offset_hz: lo_offset_for(current_rate, lo_offset, current_mode),
            clip_guard: clip_guard_on,
            tuner_agc: tuner_agc_on,
            processing,
            zoom: current_zoom,
            bandwidth_hz: f64::from(current_mode.bandwidth_hz()),
            passband_offset_hz: f64::from(current_mode.passband_offset_hz()),
            spurs_hz: Vec::new(),
            freq_error_hz: None,
            pager_sweep: None,
            pager_live_hz: None,
            scan: None,
            flex_diag: None,
            avg: AvgMode::Off.wire().into(),
            digital_cfg: Some(crate::hfdata::config()),
            ..SdrStatus::default()
        };
        stamp_filter(&mut s, current_mode);
        describe_radio(&mut s, &dev, host);
    }

    let _ = dev.cmd.send(Cmd::ClipGuard(clip_guard_on));
    let _ = dev.cmd.send(Cmd::TunerAgc(tuner_agc_on));
    tuned_freq = current_freq;
    let rx = dev.iq.subscribe_with_depth(32);
    let mut dev_opt = Some(dev);
    let mut rx_opt = Some(rx);

    let mut spectrum = Spectrum::new(fft_size);
    // The zoomed window's own transform, built when the zoom goes past 1x
    // and rebuilt whenever the rate, the window width or the display width
    // change its plan (see ZoomPath).
    let mut zoom: Option<ZoomPath> = None;
    let mut pwr: Vec<f32> = Vec::with_capacity(fft_size);
    // Per-bin peak of each frame's segments. Always sent: whether the
    // waterfall paints from it is each browser's own choice (WF PEAK), and
    // a shared switch would let one viewer turn it off under another.
    let mut pwr_peak: Vec<f32> = Vec::with_capacity(fft_size);
    let mut smoothed: Vec<f32> = Vec::with_capacity(fft_size);
    // One sorted copy of the displayed spectrum per frame, shared by
    // channel_level, carrier_offset_hz and find_peaks. Each used to clone and
    // sort its own at 25 fps.
    let mut sorted_scratch: Vec<f32> = Vec::with_capacity(fft_size);
    // Per-bin minimum-statistics noise floor. Falls onto noise promptly and
    // rises only slowly under traffic, so a persistent carrier never becomes
    // its own reference. This is the canonical floor: peak detection reads it
    // rather than the jittery single-frame percentile.
    let mut bin_floor = scannerd_dsp::NoiseFloor::new();
    // Display integration state (weak-signal aid): exponentially averaged
    // trace and a slowly-decaying max-hold.
    let mut avg_mode = AvgMode::Off;
    let mut averaged: Vec<f32> = Vec::with_capacity(fft_size);
    let mut max_hold: Vec<f32> = Vec::with_capacity(fft_size);
    // Weak-signal audio conditioning on the monitor path: spectral noise
    // gate (opens when the high-passed discriminator energy drops, i.e. the
    // channel is quieting) plus hang leveler, so static between words stops
    // burying a marginal carrier. Always on for NFM/PACKET/AUTO — it is the
    // listening equivalent of the decoders' noise gates. Built below, once
    // the inspect chain exists to rate it against.
    // PAGER: walking POCSAG+FLEX bank over the recorded span. Built lazily on
    // first Pager frame, rebuilt when the span rate changes.
    let mut pager_bank: Option<scannerd_engine::pager_bank::PagerBank> = None;
    let mut ais_rx: Option<scannerd_engine::ais::AisReceiver> = None;
    let mut ais_key: Option<(f64, f64)> = None;
    let mut ais_frames: Vec<scannerd_engine::ais::AisFrame> = Vec::new();
    let mut ais_logged = Instant::now();
    let mut pager_rate_cache = 0.0f64;
    let mut pager_live_audio: Vec<f32> = Vec::new();
    // Whether the pager sweep readout needs republishing (dwell stepped or
    // bank rebuilt).
    let mut pager_sweep_dirty = true;
    let mut block_ms: u64;
    // Frequencies that have actually decoded something. Peak markers on the
    // spectrum get their protocol label from here.
    let mut decode_history = DecodeHistory::new();
    // Residual carrier tracking for the narrowband inspect channel. The
    // clicked frequency is only as good as the ppm correction and the click;
    // a couple of kHz of error leaves a POCSAG/FLEX deviation pushing through
    // the skirt of the channel filter and syncs get missed. The AFC steers
    // the mix NCO so the carrier rides centred. It only integrates while a
    // signal is actually present, so an idle channel cannot walk it away.
    // The ±6.5 kHz range fits comfortably inside the 25 kHz packet channel:
    // at 900 MHz an RTL crystal drifts several ppm between calibrations, and
    // the old narrow acquisition path left the outer deviation sliced by the
    // filter skirt — POCSAG text decoded, but garbled.
    let mut afc = scannerd_engine::Afc::new(0.0, 6_500.0).with_alpha(0.10);
    // Last correction actually applied to the chain NCO, so the offset write
    // happens only when the loop has moved meaningfully.
    let mut afc_last_reported = 0.0f32;
    // Discriminator DC from the previous classifier pass — the residual
    // carrier error the AFC folds in.
    let mut last_center_offset_hz = 0.0f32;

    // Voice scan: the configuration it runs with, and the state machine that
    // only exists while SCAN is the mode. `scan_t0` feeds the scanner's
    // injected clock; the worker hands it elapsed seconds, never Instant,
    // so the machine stays testable without hardware.
    // Normalized here, not only inside the Scanner: the worker sizes the
    // slot pool and the per-block evidence vector from this copy, and the
    // pool length has to agree with the machine's own.
    let mut scan_cfg = cfg
        .scan
        .clone()
        .map(|c| c.normalized())
        .unwrap_or_default();
    let mut scanner: Option<Scanner> = None;
    let scan_t0 = Instant::now();
    // A restart with mode = "scan" saved comes straight up scanning: the
    // machine normally exists only from a Mode command, but there is no
    // command when the mode was never anything else.
    if current_mode == SdrMode::Scan {
        scanner = Some(Scanner::new(scan_cfg.clone()));
    }
    // The scanner's parallel test pool: one rig per configured slot, built
    // when the scanner's Inspect actions name them. Idle slots are `None`.
    let mut scan_slots: Vec<Option<ScanSlotRig>> = Vec::new();
    // Per-slot evidence for the current block, handed to the scanner in one
    // call so simultaneous locks arbitrate against each other.
    let mut scan_signals: Vec<Option<ScanSignal>> = Vec::new();
    // Actions the scanner asked for during the mode arm, executed right
    // after the match (it must not re-enter the chain mid-arm).
    let mut scan_actions: Vec<crate::scan::Action> = Vec::new();
    // Sample rate of what the scan arm last routed into audio_buf. This
    // CANNOT be read back off the demodulator at recording time: a receiver
    // that has just gone out of call reports the chain rate again, and a
    // call of 8 kHz voice stamped 48 kHz replays six times too fast.
    let mut scan_audio_rate = 0.0f64;
    // The slot the lock was last followed and re-centred for: one coarse
    // follow (shared chain to the call) and one fine nudge (receiver to its
    // own carrier measurement) per call.
    let mut scan_nudged_for: Option<usize> = None;
    // The fine nudge needs its own gate: the receiver's offset estimate is a
    // running average that survives `retune()`, so without one the stale
    // average keeps clearing the threshold and the same correction is applied
    // block after block, walking the receiver off the carrier it had found.
    let mut scan_fine_nudged_for: Option<usize> = None;
    // Voice audio accumulating for the current call — shared by the
    // P25/DMR arm and the scanner (which also records analog calls on
    // squelch edges); the modes are exclusive, so one recorder serves both.
    let mut rec_voice = CallRecorder::new();
    // Per-slot call recorders for the voice scan: every slot in a call
    // records its own audio, whether or not it is the one on the speaker.
    // Resized with the slot pool.
    let mut scan_recs: Vec<CallRecorder> = Vec::new();
    // Mode and sample rate stamped when each per-slot recorder started, so
    // a call that ends without its own close event (a release, a reseat, a
    // skip) still files with the rate the audio actually arrived at.
    let mut scan_rec_meta: Vec<Option<(crate::scan::ScanMode, u32)>> = Vec::new();

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
    let mut front = FrontEnd::with_blanker(current_rate, processing.blanker);
    let mut clean: Vec<num_complex::Complex32> = Vec::new();
    let mut demod = Demod::new(current_mode, inspect_chain.fs_out(), inspect_hz);
    {
        let mut s = status.lock().unwrap();
        s.inspect_rate_hz = inspect_chain.fs_out();
        s.audio_rate_hz = demod.audio_rate(inspect_chain.fs_out());
    }
    let _ = events.send(SdrEvent::Status(status.lock().unwrap().clone()));
    // A restart with mode = "scan" saved comes straight up scanning: the slot
    // pool exists from the same moment the scanner does.
    if current_mode == SdrMode::Scan {
        scan_slots = (0..scan_cfg.slots).map(|_| None).collect();
    }
    let mut inspect_iq = Vec::new();
    let mut notch_scratch: Vec<num_complex::Complex32> = Vec::new();
    // Cached channel notches: (spur offsets the notches were built for, NCOs).
    // Rebuilt only when the spur geometry or sample rate changes.
    let mut notch_cache: Option<(Vec<f64>, Vec<ChannelNotch>)> = None;
    // Instantaneous frequency of the inspected channel, for the eye and
    // constellation displays. The protocol receivers keep their own symbol
    // recovery private, and this only has to be good enough to look at.
    let mut scope_fm = NbfmDemod::with_deviation(inspect_chain.fs_out(), 3_000.0);
    // The monitor chain, rated for the audio it actually receives. Every
    // path that runs these feeds them chain-rate audio — the digital voice
    // receivers bypass this conditioning entirely — and the chain's output
    // rate is span/round(span/48 kHz), never a round number. Built for a
    // nominal 8 kHz, the gate's sample-counted timers (close delay, ambiguity
    // latch) ran ~6× short in real time and chopped marginal carriers into
    // fragments.
    let mut mon_gate = NoiseGate::new(inspect_chain.fs_out());
    let mut mon_leveler = Leveler::new(inspect_chain.fs_out());
    let mut mon_notch = AutoNotch::new(inspect_chain.fs_out());
    let mut mon_denoise = Denoise::new(inspect_chain.fs_out());
    mon_denoise.set_level(processing.nr);
    let mut mon_rate_cache = inspect_chain.fs_out();
    let mut scope_disc: Vec<f32> = Vec::new();
    // Whatever this frame's scope trace is, normalised to ±1.
    let mut scope_src: Vec<f32> = Vec::new();
    let mut scope_src_rate;
    // Full-scale deviation of whatever `scope_src` holds, so a normalised
    // trace can be turned back into Hz for the signal readout.
    let mut scope_dev_scale = 0.0f32;
    // Last measured channel SNR, handed to the receivers for their summaries.
    let mut last_snr_db = 0.0f32;
    let mut last_lagged_seen = 0u64;
    let mut last_lag_at: Option<Instant> = None;
    // Smoothed carrier error. A single frame's centroid is noisy; a
    // calibration wants a settled figure.
    let mut freq_error_hz: Option<f64> = None;
    let mut last_carrier_err_at: Option<Instant> = None;
    let mut last_carrier_inspect_hz: f64 = inspect_hz;
    let mut flex_carrier_corr_hz: f64 = 0.0;
    let mut last_flex_inspect_hz: f64 = inspect_hz;
    let mut next_call_id: u64 = 1;
    // Audio for the browser, refilled each block. Reused so a steady stream of
    let mut audio_buf: Vec<f32> = Vec::new();

    // Display frames are paced on the *sample* clock, not the wall clock.
    // The radio delivers ~20 ms blocks; a wall-clock gate measured from the
    // previous frame's emission (which lands a few ms into a block, after
    // the DSP for it) sees only ~37 ms at the second block and waits for a
    // third, so the stream ran at 20 fps with 40/60 ms jitter. Counting
    // samples makes it exactly every `rate / FPS` samples — two blocks —
    // regardless of how long the previous block took to process.
    let mut frame_pending_samples: u64 = 0;
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
    let mut _last_tune_ok = Instant::now();
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
    // Sample-continuity bookkeeping for this loop's subscriber: every
    // accepted IQ block carries its source position and acquisition epoch,
    // so a lost delivery is visible at the next successful block (exact
    // lost-sample count) and stale blocks queued before an accepted
    // tune/rate change are identifiable instead of being framed across.
    // Re-created wherever the subscription itself is replaced (a device
    // switch or reopen starts a fresh epoch timeline from zero).
    let mut continuity = scannerd_radio::ContinuityTracker::new();
    let mut lost_samples = 0u64;
    let mut gap_events = 0u64;
    // Set by command handlers that want the inspect chain re-mixed, applied
    // once after the drain: a UI burst of Tune/Inspect/Rate/Mode in one batch
    // is one chain rebuild (FIR design + FFT plan + capture clear), not five.
    let mut inspect_dirty = false;
    let mut sub_rx: Option<crate::sub_rx::SubRx> = None;

    while !stop.load(Ordering::Relaxed) {
        // Drain commands
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                SdrCmd::Tune(f) => {
                    // The operator has taken the dial: the scan pauses rather
                    // than fighting the click.
                    if let Some(sc) = scanner.as_mut() {
                        scan_actions.extend(sc.pause("the dial was moved by hand"));
                    }
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
                        // Coalesced: rebuilt once after the drain, below.
                        inspect_dirty = true;
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
                    // The operator has taken the dial: the scan pauses rather
                    // than fighting the click.
                    if let Some(sc) = scanner.as_mut() {
                        scan_actions.extend(sc.pause("the dial was moved by hand"));
                    }
                    // Coalesced: rebuilt once after the drain, below.
                    inspect_dirty = true;
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
                        // The LO offset depends on the span too: the tuner
                        // follows it, as it does on a mode change.
                        match lo_move_between((current_rate, current_mode), (r, current_mode), lo_offset) {
                            LoMove::Reopen => force_reopen_now = true,
                            LoMove::Retune => pending_tune = Some(current_freq),
                            LoMove::Stay => {}
                        }
                        current_rate = r;
                        if let Some(ref dev) = dev_opt {
                            let _ = dev.cmd.send(Cmd::Rate(r));
                        }
                        // Every scan window boundary just moved, and every
                        // slot rig's chain rate with it.
                        if let Some(sc) = scanner.as_mut() {
                            for a in sc.set_span(r) {
                                apply_scan_action(
                                    a,
                                    &mut InspectParts {
                                        inspect_chain: &mut inspect_chain,
                                        classifier: &mut classifier,
                                        demod: &mut demod,
                                        scope_fm: &mut scope_fm,
                                        capture: &capture,
                                        afc: &mut afc,
                                        afc_last_reported: &mut afc_last_reported,
                                    },
                                    &mut scan_slots,
                                    &events,
                                    &status,
                                    &mut current_freq,
                                    tuned_freq,
                                    &mut inspect_hz,
                                    &mut pending_tune,
                                    current_rate,
                                    lo_offset,
                                    current_mode,
                                    &scan_cfg.modes,
                                );
                            }
                        }
                        inspect_hz = inspect_hz.clamp(
                            current_freq
                                - display_rate_for(current_rate, lo_offset, current_mode) * 0.49,
                            current_freq
                                + display_rate_for(current_rate, lo_offset, current_mode) * 0.49,
                        );
                        // Coalesced: rebuilt once after the drain, below.
                        inspect_dirty = true;
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
                        match lo_move(current_rate, lo_offset, current_mode, m) {
                            LoMove::Reopen => force_reopen_now = true,
                            // The chain is about to be mixed for the new
                            // distance, so the tuner has to move there too.
                            LoMove::Retune => pending_tune = Some(current_freq),
                            LoMove::Stay => {}
                        }
                        current_mode = m;
                        // Coalesced: rebuilt once after the drain, below.
                        inspect_dirty = true;
                        let mut s = status.lock().unwrap();
                        s.mode = current_mode;
                        stamp_filter(&mut s, current_mode);
                        s.lo_offset_hz = lo_offset_for(current_rate, lo_offset, current_mode);
                        let shown = display_rate_for(current_rate, lo_offset, current_mode);
                        s.min_freq_hz = current_freq - shown / 2.0;
                        s.max_freq_hz = current_freq + shown / 2.0;
                        s.audio_rate_hz = demod.audio_rate(inspect_chain.fs_out());
                        s.inspect_rate_hz = inspect_chain.fs_out();
                        // The scanner exists only while SCAN is the mode:
                        // entering SCAN builds it from the current config,
                        // leaving SCAN tears it down — pool and all.
                        scanner = (current_mode == SdrMode::Scan)
                            .then(|| Scanner::new(scan_cfg.clone()));
                        if current_mode == SdrMode::Scan {
                            scan_slots = (0..scan_cfg.slots).map(|_| None).collect();
                        } else {
                            // Leaving SCAN mid-call: the recorders file what
                            // they caught rather than silently discarding an
                            // in-progress call's audio with the pool.
                            for (i, rec) in scan_recs.iter_mut().enumerate() {
                                let freq = scan_slots
                                    .get(i)
                                    .and_then(|s| s.as_ref())
                                    .map(|s| s.freq_hz)
                                    .unwrap_or(inspect_hz);
                                let meta = scan_rec_meta.get(i).copied().flatten();
                                flush_scan_rec(
                                    rec,
                                    meta,
                                    freq,
                                    (inspect_chain.fs_out() as u32).max(1),
                                    &calls,
                                    &mut next_call_id,
                                );
                            }
                            scan_slots.clear();
                        }
                        s.scan = scanner.as_ref().map(|sc| sc.status(scan_t0.elapsed().as_secs_f64()));
                        if current_mode != SdrMode::Pager {
                            s.pager_sweep = None;
                            s.pager_live_hz = None;
                        }
                        pager_sweep_dirty = true;
                        let _ = events.send(SdrEvent::Status(s.clone()));
                    }
                }
                SdrCmd::ScanConfig(new_cfg) => {
                    scan_cfg = new_cfg.normalized();
                    if let Some(sc) = scanner.as_mut() {
                        for a in sc.set_cfg(scan_cfg.clone()) {
                            apply_scan_action(
                                a,
                                &mut InspectParts {
                                    inspect_chain: &mut inspect_chain,
                                    classifier: &mut classifier,
                                    demod: &mut demod,
                                    scope_fm: &mut scope_fm,
                                    capture: &capture,
                                    afc: &mut afc,
                                    afc_last_reported: &mut afc_last_reported,
                                },
                                &mut scan_slots,
                                &events,
                                &status,
                                &mut current_freq,
                                tuned_freq,
                                &mut inspect_hz,
                                &mut pending_tune,
                                current_rate,
                                lo_offset,
                                current_mode,
                                &scan_cfg.modes,
                            );
                        }
                    }
                    let mut s = status.lock().unwrap();
                    s.scan = scanner.as_ref().map(|sc| sc.status(scan_t0.elapsed().as_secs_f64()));
                    let _ = events.send(SdrEvent::Status(s.clone()));
                }
                SdrCmd::ScanControl(ctl, freq) => {
                    use crate::scan::ScanControl as Ctl;
                    let now = scan_t0.elapsed().as_secs_f64();
                    let mut acts: Vec<crate::scan::Action> = Vec::new();
                    if let Some(sc) = scanner.as_mut() {
                        match ctl {
                            Ctl::Pause => acts.extend(sc.pause("paused by the operator")),
                            Ctl::Resume => acts = sc.resume(),
                            Ctl::Skip => acts = sc.skip(now),
                            Ctl::Forget => sc.forget(),
                            Ctl::Unskip => {
                                if let Some(f) = freq {
                                    sc.unskip(f);
                                }
                            }
                        }
                    }
                    for a in acts {
                        apply_scan_action(
                            a,
                            &mut InspectParts {
                                inspect_chain: &mut inspect_chain,
                                classifier: &mut classifier,
                                demod: &mut demod,
                                scope_fm: &mut scope_fm,
                                capture: &capture,
                                afc: &mut afc,
                                afc_last_reported: &mut afc_last_reported,
                            },
                            &mut scan_slots,
                            &events,
                            &status,
                            &mut current_freq,
                            tuned_freq,
                            &mut inspect_hz,
                            &mut pending_tune,
                            current_rate,
                            lo_offset,
                            current_mode,
                            &scan_cfg.modes,
                        );
                    }
                    let mut s = status.lock().unwrap();
                    s.scan = scanner.as_ref().map(|sc| sc.status(scan_t0.elapsed().as_secs_f64()));
                    let _ = events.send(SdrEvent::Status(s.clone()));
                }
                SdrCmd::DisplayBins(bins) => {
                    // Every viewer shares one FFT, and the last to report
                    // sets it: so a wide screen may raise it, but a phone
                    // never takes resolution away from everyone else.
                    let bins = bins.clamp(ZOOM_TARGET_BINS, 4096).next_power_of_two();
                    if bins != display_bins {
                        display_bins = bins;
                        // The zoom path picks the new width up on the next
                        // block, when its plan is compared.
                        let want = fft_size_for(display_bins);
                        if want != fft_size {
                            fft_size = want;
                            spectrum = Spectrum::new(fft_size);
                            pwr = Vec::with_capacity(fft_size);
                            smoothed = Vec::with_capacity(fft_size);
                        }
                        let mut s = status.lock().unwrap();
                        s.fft_size = displayed_fft_size(fft_size, current_zoom, current_rate, lo_offset, current_mode, display_bins);
                        let _ = events.send(SdrEvent::Status(s.clone()));
                    }
                }
                SdrCmd::Zoom(z) => {
                    let z = z.clamp(1.0, ZOOM_MAX);
                    if (z - current_zoom).abs() > 0.001 {
                        current_zoom = z;
                        // A zoom centers on the receiver, held or not; a
                        // held window then stays there.
                        zoom_view.center_hz = inspect_hz;
                        // The zoom path is (re)built on the next block from
                        // the new window; the full-span FFT does not change.
                        let mut s = status.lock().unwrap();
                        s.zoom = current_zoom;
                        s.fft_size = displayed_fft_size(fft_size, current_zoom, current_rate, lo_offset, current_mode, display_bins);
                        let shown = display_rate_for(current_rate, lo_offset, current_mode)
                            / current_zoom;
                        s.min_freq_hz = inspect_hz - shown / 2.0;
                        s.max_freq_hz = inspect_hz + shown / 2.0;
                        let _ = events.send(SdrEvent::Status(s.clone()));
                    }
                }
                SdrCmd::ZoomHold { hold, center_hz } => {
                    zoom_view.hold = hold;
                    zoom_view.seen_inspect_hz = inspect_hz;
                    if hold {
                        zoom_view.center_hz = center_hz.unwrap_or(shown_center_hz);
                    }
                    let mut s = status.lock().unwrap();
                    s.zoom_hold = hold;
                    let _ = events.send(SdrEvent::Status(s.clone()));
                }
                SdrCmd::Rx2(want) => {
                    sub_rx = want.map(|(hz, m)| crate::sub_rx::SubRx::new(hz, m));
                    let mut s = status.lock().unwrap();
                    s.rx2 = sub_rx.as_ref().map(|r| r.status());
                    let _ = events.send(SdrEvent::Status(s.clone()));
                }
                SdrCmd::Digital { fax_start, fax_stop } => {
                    let cfg = crate::hfdata::config();
                    if let Some(hf) = demod.hf_mut() {
                        hf.apply(&cfg, inspect_chain.fs_out());
                        hf.fax(fax_start, fax_stop);
                    }
                    let mut s = status.lock().unwrap();
                    s.digital_cfg = Some(cfg);
                    let _ = events.send(SdrEvent::Status(s.clone()));
                }
                SdrCmd::Filter => {
                    inspect_dirty = true;
                    // The second receiver rebuilds around its mode's filter
                    // on its next block.
                    if let Some(sub) = sub_rx.as_mut() {
                        sub.invalidate();
                    }
                    let mut s = status.lock().unwrap();
                    s.rx2 = sub_rx.as_ref().map(|r| r.status());
                    stamp_filter(&mut s, current_mode);
                    let _ = events.send(SdrEvent::Status(s.clone()));
                }
                SdrCmd::Avg(mode) => {
                    avg_mode = mode;
                    // In the status so every client shows the same setting,
                    // not whatever it last sent.
                    let mut s = status.lock().unwrap();
                    s.avg = mode.wire().to_string();
                    let _ = events.send(SdrEvent::Status(s.clone()));
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
                SdrCmd::Processing(p) => {
                    processing = p;
                    front.set_blanker(p.blanker);
                    mon_denoise.set_level(p.nr);
                    let mut s = status.lock().unwrap();
                    s.processing = p;
                    let _ = events.send(SdrEvent::Status(s.clone()));
                }
                SdrCmd::TunerAgc(on) => {
                    tuner_agc_on = on;
                    if let Some(ref dev) = dev_opt {
                        let _ = dev.cmd.send(Cmd::TunerAgc(on));
                    }
                    let mut s = status.lock().unwrap();
                    s.tuner_agc = on;
                    let _ = events.send(SdrEvent::Status(s.clone()));
                }
                SdrCmd::BiasT(on) => {
                    // Persist against this radio's serial: whether the port
                    // needs power is a fact about the feedline on it, like
                    // its ppm is about its crystal. Saved while the radio is
                    // offline too, so the next open applies it.
                    if let Some(serial) = current_serial.clone() {
                        let mut cfg = config::load().unwrap_or_default();
                        let tuner = status.lock().unwrap().tuner.clone();
                        cfg.entry(&serial, &tuner).bias_t = on;
                        if let Err(e) = config::save(&cfg) {
                            eprintln!("could not save bias-T: {e:#}");
                        }
                    }
                    if let Some(ref dev) = dev_opt.as_ref().filter(|d| d.caps.bias_t) {
                        let _ = dev.cmd.send(Cmd::BiasT(on));
                        // Shown at once; the device's readback confirms it.
                        let mut s = status.lock().unwrap();
                        s.bias_t = Some(on);
                        let _ = events.send(SdrEvent::Status(s.clone()));
                    }
                }
                SdrCmd::RfNotch(on) => {
                    notches.rf = Some(on);
                    if let Some(ref dev) = dev_opt.as_ref().filter(|d| d.caps.rf_notch) {
                        let _ = dev.cmd.send(Cmd::RfNotch(on));
                        let mut s = status.lock().unwrap();
                        s.rf_notch = Some(on);
                        let _ = events.send(SdrEvent::Status(s.clone()));
                    }
                }
                SdrCmd::DabNotch(on) => {
                    notches.dab = Some(on);
                    if let Some(ref dev) = dev_opt.as_ref().filter(|d| d.caps.dab_notch) {
                        let _ = dev.cmd.send(Cmd::DabNotch(on));
                        let mut s = status.lock().unwrap();
                        s.dab_notch = Some(on);
                        let _ = events.send(SdrEvent::Status(s.clone()));
                    }
                }
                // Driver settings with no state of their own here: the radio
                // reads them back and the State event updates the status.
                SdrCmd::AgcSetpoint(db) => {
                    if let Some(ref dev) = dev_opt.as_ref().filter(|d| d.caps.agc_setpoint) {
                        let _ = dev.cmd.send(Cmd::AgcSetpoint(db));
                    }
                }
                SdrCmd::IqCorrection(on) => {
                    if let Some(ref dev) = dev_opt.as_ref().filter(|d| d.caps.iq_correction) {
                        let _ = dev.cmd.send(Cmd::IqCorrection(on));
                    }
                }
                SdrCmd::IfBandwidth(hz) => {
                    if let Some(ref dev) = dev_opt.as_ref() {
                        let _ = dev.cmd.send(Cmd::IfBandwidth(hz));
                    }
                }
                SdrCmd::LoOffset(on) => {
                    lo_offset_chosen = true;
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
                            &remotes,
                            Some(&new_serial),
                            current_freq,
                            current_rate,
                            current_gain,
                            LoRequest { on: &mut lo_offset, chosen: lo_offset_chosen, mode: current_mode },
                            notches,
                        )
                        {
                            Ok((new_dev, s_serial, s_tuner, s_host)) => {
                                // The new radio starts from the operator's
                                // front-end choices, as a reopen does.
                                let _ = new_dev.cmd.send(Cmd::ClipGuard(clip_guard_on));
                                let _ = new_dev.cmd.send(Cmd::TunerAgc(tuner_agc_on));
                                current_serial = Some(s_serial.clone());
                                // The new device may have clamped the rate;
                                // rebuild the inspect path for what it
                                // actually delivers.
                                current_rate = new_dev.rate;
                                let new_rx = new_dev.iq.subscribe_with_depth(32);
                                dev_opt = Some(new_dev);
                                rx_opt = Some(new_rx);
                                // New subscription: its epoch timeline starts
                                // from nothing, not from the old device's.
                                continuity = scannerd_radio::ContinuityTracker::new();
                                let ppm = config::load().unwrap_or_default().ppm_for(&s_serial);
                                remix_inspect(
                                    &mut InspectParts {
                                        inspect_chain: &mut inspect_chain,
                                        classifier: &mut classifier,
                                        demod: &mut demod,
                                        scope_fm: &mut scope_fm,
                                        capture: &capture,
                                        afc: &mut afc,
                                        afc_last_reported: &mut afc_last_reported,
                                    },
                                    current_rate,
                                    current_freq,
                                    inspect_hz,
                                    lo_offset_for(current_rate, lo_offset, current_mode),
                                    current_mode,
                                );
                                let mut s = status.lock().unwrap();
                                if let Some(ref d) = dev_opt {
                                    describe_radio(&mut s, d, s_host);
                                }
                                s.serial = s_serial;
                                s.tuner = s_tuner;
                                s.ppm = ppm;
                                s.rate_hz = current_rate;
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

        // One re-mix per loop pass, however many commands asked for one. The
        // handlers above only read/write scalars, so deferring the chain
        // rebuild to here changes nothing about the values it consumes: they
        // are the post-drain, post-batch ones.
        if inspect_dirty {
            inspect_dirty = false;
            remix_inspect(
                &mut InspectParts {
                    inspect_chain: &mut inspect_chain,
                    classifier: &mut classifier,
                    demod: &mut demod,
                    scope_fm: &mut scope_fm,
                    capture: &capture,
                    afc: &mut afc,
                    afc_last_reported: &mut afc_last_reported,
                },
                current_rate,
                current_freq,
                inspect_hz,
                lo_offset_for(current_rate, lo_offset, current_mode),
                current_mode,
            );
        }

        // Zoomed, keep the LO's notched DC residue out of the window: the
        // tuner moves so it sits just past the window's edge. Not while the
        // scanner owns the tuner.
        if scanner.is_none() {
            let view = if zoom_view.hold && current_zoom > 1.0 { zoom_view.center_hz } else { inspect_hz };
            let mut receivers = vec![inspect_hz];
            receivers.extend(sub_rx.as_ref().map(|r| r.status().freq_hz));
            if let Some(f) = lo_clear_freq(current_rate, lo_offset, current_mode, current_zoom, current_freq, view, &receivers) {
                current_freq = f;
                pending_tune = Some(f);
                remix_inspect(
                    &mut InspectParts {
                        inspect_chain: &mut inspect_chain,
                        classifier: &mut classifier,
                        demod: &mut demod,
                        scope_fm: &mut scope_fm,
                        capture: &capture,
                        afc: &mut afc,
                        afc_last_reported: &mut afc_last_reported,
                    },
                    current_rate,
                    current_freq,
                    inspect_hz,
                    lo_offset_for(current_rate, lo_offset, current_mode),
                    current_mode,
                );
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
                            let _ = dev.cmd.send(Cmd::Gain(db));
                            agc_on = false;
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
                &remotes,
                current_serial.as_deref(),
                current_freq,
                current_rate,
                current_gain,
                LoRequest { on: &mut lo_offset, chosen: lo_offset_chosen, mode: current_mode },
                notches,
            ) {
                Ok((new_dev, s_serial, s_tuner, s_host)) => {
                    let _ = new_dev.cmd.send(Cmd::ClipGuard(clip_guard_on));
                    let _ = new_dev.cmd.send(Cmd::TunerAgc(tuner_agc_on));
                    // open() tunes as it goes, so the reopened device is
                    // genuinely where it was asked to be.
                    tuned_freq = current_freq;
                    _last_tune_ok = Instant::now();
                    // The reopened device may have clamped the rate; rebuild
                    // the inspect path for what it actually delivers.
                    current_rate = new_dev.rate;
                    let new_rx = new_dev.iq.subscribe_with_depth(32);
                    dev_opt = Some(new_dev);
                    rx_opt = Some(new_rx);
                    // New subscription: its epoch timeline starts from
                    // nothing, not from the faulted device's.
                    continuity = scannerd_radio::ContinuityTracker::new();
                    let ppm = config::load().unwrap_or_default().ppm_for(&s_serial);
                    current_serial = Some(s_serial.clone());
                    // The reopened dongle is a fresh chain: anything buffered
                    // from before the fault was taken at an unknown tuning.
                    remix_inspect(
                        &mut InspectParts {
                            inspect_chain: &mut inspect_chain,
                            classifier: &mut classifier,
                            demod: &mut demod,
                            scope_fm: &mut scope_fm,
                            capture: &capture,
                            afc: &mut afc,
                            afc_last_reported: &mut afc_last_reported,
                        },
                        current_rate,
                        current_freq,
                        inspect_hz,
                        lo_offset_for(current_rate, lo_offset, current_mode),
                        current_mode,
                    );
                    if deliberate {
                        eprintln!("SDR {s_serial}: reopened to change the LO offset");
                    } else {
                        eprintln!(
                            "SDR {s_serial}: reopened after {}",
                            if wedged { "a wedged tuner" } else { "a USB fault" }
                        );
                    }
                    let mut s = status.lock().unwrap();
                    if let Some(ref d) = dev_opt {
                        describe_radio(&mut s, d, s_host);
                    }
                    s.serial = s_serial;
                    s.tuner = s_tuner;
                    s.ppm = ppm;
                    s.rate_hz = current_rate;
                    // It reopened where it was asked to be, which is not
                    // where the status last said if a tune came in while it
                    // was down — a radio across a network is down more often
                    // than one on USB.
                    let shown = display_rate_for(current_rate, lo_offset, current_mode);
                    s.freq_hz = current_freq;
                    s.min_freq_hz = current_freq - shown / 2.0;
                    s.max_freq_hz = current_freq + shown / 2.0;
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
                if ctl_faults.len() >= CTL_FAULT_LIMIT {
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
                // The RSP's gain as its IF AGC and the LNA's take-over move
                // it, each second it changes.
                scannerd_radio::Event::Gain(g) => {
                    if let Ok(mut st) = status.lock() {
                        let now = (g.overall, g.rfgr, g.ifgr, g.overload, g.overload_count);
                        if now != (st.gain_now_db, st.rfgr_db, st.ifgr_db, st.overload, st.overload_count) {
                            (st.gain_now_db, st.rfgr_db, st.ifgr_db, st.overload, st.overload_count) = now;
                            let _ = events.send(SdrEvent::Status(st.clone()));
                        }
                    }
                }
                scannerd_radio::Event::State(state) => {
                    if let Ok(mut st) = status.lock() {
                        st.gain_now_db = state.overall_gain;
                        // The gain range follows the band on an SDRplay, and
                        // the switches are what the driver read back: worth
                        // a status frame of their own when they move.
                        let range = state.gain_range.unwrap_or((st.gain_min_db, st.gain_max_db));
                        let moved = range != (st.gain_min_db, st.gain_max_db)
                            || (state.bias_t, state.rf_notch, state.dab_notch)
                                != (st.bias_t, st.rf_notch, st.dab_notch)
                            || (state.iq_correction, state.bandwidth_manual)
                                != (st.iq_correction, st.if_bandwidth_manual)
                            || state.agc_setpoint != st.agc_setpoint_dbfs
                            || state.bandwidth != st.if_bandwidth_hz
                            || (state.rfgr, state.ifgr) != (st.rfgr_db, st.ifgr_db)
                            || (state.lna_auto, state.overload) != (st.lna_auto, st.overload)
                            || state.lna_steps_db != st.lna_steps_db;
                        (st.gain_min_db, st.gain_max_db) = range;
                        st.bias_t = state.bias_t;
                        st.rf_notch = state.rf_notch;
                        st.dab_notch = state.dab_notch;
                        st.agc_setpoint_dbfs = state.agc_setpoint;
                        st.iq_correction = state.iq_correction;
                        st.rfgr_db = state.rfgr;
                        st.ifgr_db = state.ifgr;
                        st.lna_steps_db = state.lna_steps_db;
                        st.lna_auto = state.lna_auto;
                        st.overload = state.overload;
                        st.overload_count = state.overload_count;
                        st.if_bandwidth_hz = state.bandwidth;
                        st.if_bandwidth_manual = state.bandwidth_manual;
                        if moved {
                            let _ = events.send(SdrEvent::Status(st.clone()));
                        }
                    }
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
                        _last_tune_ok = Instant::now();
                        ctl_faults.clear();
                        tuned_freq = hw - lo_off;
                        // Keep the AFC's accumulated correction: this event
                        // confirms a tune the operator (or scanner) asked
                        // for, not a re-mix of the same channel, so wiping
                        // the correction would drop a locked carrier back
                        // onto a filter skirt until the loop re-learns it.
                        inspect_chain.set_offset(inspect_hz - hw + f64::from(afc.correction_hz()));
                        // The offset already carries the correction; keep the
                        // loop's bookkeeping in step so it does not re-apply.
                        afc_last_reported = afc.correction_hz();
                        // Every digital receiver rides a narrowband chain —
                        // the demod's on the inspect chain above, each scan
                        // slot's on its own — so re-pointing those chains at
                        // the real LO carries every receiver with it. No
                        // per-receiver retune exists to do any more.
                        for slot in scan_slots.iter_mut().flatten() {
                            slot.chain.set_offset(slot.freq_hz - hw);
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
        // Sample-continuity gate: every block carries its source position
        // and acquisition epoch. A stale block (queued before an accepted
        // tune/rate change on the old epoch) never reaches a decode path;
        // a gap (lost deliveries) or an epoch boundary resets the framing
        // state so nothing splices samples across the hole.
        let gap_report = continuity.observe(&block);
        if gap_report.stale {
            continue;
        }
        if gap_report.lost_samples > 0 {
            lost_samples += gap_report.lost_samples;
            gap_events += 1;
        }
        let discontinuous = gap_report.epoch_start || gap_report.lost_samples > 0;
        if discontinuous {
            // The sample timeline broke before this block. Drop every
            // streaming state that would otherwise stitch what came before
            // the hole onto what comes after — framing decoders would see a
            // waveform that never existed on the air. The AFC correction is
            // deliberately kept: it describes the carrier, not the sample
            // run, and wiping it would drop a locked weak carrier back onto
            // a filter skirt until the loop re-learns it (the same reason
            // the accepted-tune path keeps it).
            front.reset();
            spectrum.clear();
            if let Some(z) = zoom.as_mut() {
                z.reset();
            }
            inspect_chain.reset();
            classifier.reset();
            demod.reset_timeline(inspect_chain.fs_out());
            scope_fm.reset();
            // Receiver B listens to the same span: its filters hold the
            // run that just ended too.
            if let Some(sub) = sub_rx.as_mut() {
                sub.invalidate();
            }
            let fs_now = inspect_chain.fs_out();
            mon_gate = NoiseGate::new(fs_now);
            mon_leveler = Leveler::new(fs_now);
            mon_notch = AutoNotch::new(fs_now);
            mon_denoise = Denoise::new(fs_now);
            mon_denoise.set_level(processing.nr);
            for slot in scan_slots.iter_mut().flatten() {
                slot.chain.reset();
                if let Some(rx) = slot.p25.as_mut() {
                    rx.reset();
                }
                if let Some(rx) = slot.dmr.as_mut() {
                    rx.reset();
                }
                if let Some(am) = slot.am.as_mut() {
                    am.reset();
                }
                if let Some(fm) = slot.nbfm.as_mut() {
                    fm.reset();
                }
                slot.fm_out = Demodulated::default();
                slot.am_audio.clear();
                slot.nfm_voice.clear();
                slot.mon_gate.reset();
                slot.mon_leveler.reset();
                slot.mon_notch = AutoNotch::new(slot.chain.fs_out());
            }
            // The inspect-side capture rings (voice/discriminator/IQ) hold
            // pre-hole samples; a downloaded capture must not splice across.
            capture
                .lock()
                .expect("SDR capture")
                .clear(inspect_hz, inspect_chain.fs_out());
        }
        // Real-time milliseconds this block represents, for dwell timers.
        block_ms = (block.len() as f64 / current_rate * 1000.0).round().max(1.0) as u64;

        capture
            .lock()
            .expect("SDR capture")
            .append_span(current_rate, &block, discontinuous);

        // Eight lanes, each with its own running peak: one peak carried
        // from sample to sample is a chain every compare waits on.
        {
            let mut lanes = [peak_iq; 8];
            let chunks = block.chunks_exact(8);
            let rest = chunks.remainder();
            for c in chunks {
                for l in 0..8 {
                    let (re, im) = (c[l].re.abs(), c[l].im.abs());
                    let m = if re > im { re } else { im };
                    if m > lanes[l] {
                        lanes[l] = m;
                    }
                }
            }
            for c in rest {
                let m = c.re.abs().max(c.im.abs());
                if m > lanes[0] {
                    lanes[0] = m;
                }
            }
            peak_iq = lanes.iter().copied().fold(peak_iq, f32::max);
        }

        // DC offset, LO leakage and the IQ image are removed here, before the
        // spectrum measures the span or the inspect chain mixes out of it. The
        // block is shared with other subscribers, so correct a copy.
        clean.clear();
        clean.extend_from_slice(&block);
        front.process(&mut clean);

        // Spectrum is displayed at DEFAULT_FPS. Ingest every block so the
        // pending window stays current; FFT only when a frame is due.
        let frame_quantum = (current_rate / f64::from(DEFAULT_FPS)).round().max(1.0) as u64;
        frame_pending_samples += block.len() as u64;
        // Due at the delivery that ends nearest the frame's time. Waiting
        // for the quantum to be passed made a delivery that falls a few
        // samples short of dividing it (a USB buffer's rounding, whole
        // 4096 blocks at 8 MS/s) run one delivery long, then catch up
        // with a short frame.
        let block_samples = block.len() as u64;
        let frame_due = frame_pending_samples + block_samples / 2 >= frame_quantum;
        // The window this block's frame will show. Decided here, before the
        // samples are mixed, so the zoom path and the frame that reads it
        // agree on where the window is.
        let window = zoom_view.window(current_rate, lo_offset, current_mode, current_zoom, tuned_freq, inspect_hz);
        shown_center_hz = window.shown_centre;
        if current_zoom > 1.0 {
            let plan = zoom_plan(current_rate, window.shown_rate, display_bins);
            match zoom.as_mut() {
                Some(z) if z.plan == plan => z.retune(window.mix_offset_hz()),
                _ => zoom = Some(ZoomPath::new(plan, window.shown_rate, window.mix_offset_hz())),
            }
        } else {
            zoom = None;
        }
        // Every sample reaches the display: segments are windowed and
        // averaged as they arrive (Welch, half-overlapped), so a burst
        // between two frames is in the next one instead of thrown away. The
        // cap only bounds a late or slow frame, about 30% over full duty.
        // Zoomed in, the display reads the zoom path and the full span only
        // serves the scanner, so the full-span transform drops to a quarter
        // duty rather than costing a second full one.
        spectrum.ingest_welch(&clean);
        if let Some(z) = zoom.as_mut() {
            z.ingest(&clean);
        }
        if frame_due {
            let full_duty = (current_rate / f64::from(DEFAULT_FPS) / (fft_size as f64 / 2.0)).ceil() as usize;
            let duty = if zoom.is_some() { full_duty / 4 } else { full_duty };
            spectrum.set_max_segments_per_take((duty * 13 / 10).max(8));
            spectrum.take_dbfs(&mut pwr, Some(&mut pwr_peak));
            if let Some(z) = zoom.as_mut() {
                z.take();
            }
        }

        // Process channel for classifier. In Scan the chain is purely
        // diagnostic — it follows the locked channel and nothing reads it
        // while the sweep is between calls (the slot rigs carry the
        // receivers) — so an idle sweep skips one whole full-rate chain's
        // worth of mix-and-filter per block.
        inspect_iq.clear();
        let scan_sweeping = current_mode == SdrMode::Scan
            && scanner.as_ref().is_none_or(|sc| sc.listen().is_none());
        if !scan_sweeping {
            inspect_chain.process(&clean, &mut inspect_iq);
        }
        // The second receiver listens to the same cleaned span.
        if let Some(sub) = sub_rx.as_mut() {
            let lo_off = lo_offset_for(current_rate, lo_offset, current_mode);
            let note_before = sub.note.clone();
            if let Some((rate, samples)) = sub.process(&clean, current_rate, current_freq, lo_off) {
                let _ = audio_tx.send(AudioFrame { rx: 1, rate_hz: rate, samples, stereo: None });
            }
            if sub.note != note_before {
                let mut s = status.lock().unwrap();
                s.rx2 = Some(sub.status());
                let _ = events.send(SdrEvent::Status(s.clone()));
            }
        }
        let fs_chain = inspect_chain.fs_out();
        // The monitor chain is sample-rated, so a span-rate change re-rates
        // the chain under it: rebuild rather than keep running 8 ms-class
        // constants at the wrong cadence.
        if mon_rate_cache != fs_chain {
            mon_gate = NoiseGate::new(fs_chain);
            mon_leveler = Leveler::new(fs_chain);
            mon_notch = AutoNotch::new(fs_chain);
            mon_denoise = Denoise::new(fs_chain);
            mon_denoise.set_level(processing.nr);
            mon_rate_cache = fs_chain;
        }

        // PAGER bank lifecycle: (re)build on rate change, step on dwell expiry.
        if current_mode == SdrMode::Pager {
            let span_rate = current_rate;
            if pager_rate_cache != span_rate {
                // Half-band covers +/-480 kHz: the 929-930 MHz paging plan.
                pager_bank = Some(scannerd_engine::pager_bank::PagerBank::new(
                    span_rate,
                    PAGER_HALF_BAND_HZ,
                ));
                pager_rate_cache = span_rate;
                pager_sweep_dirty = true;
            }
            if let Some(bank) = pager_bank.as_mut()
                && bank.since_step_exceeds_ms(scannerd_engine::pager_bank::DWELL_MS)
            {
                bank.step();
                pager_sweep_dirty = true;
            }
            // Publish the sweep position whenever it moves: the operator
            // watching for pages wants to know which channel is live without
            // reading the marker off the waterfall.
            if pager_sweep_dirty {
                pager_sweep_dirty = false;
                if let Some(bank) = pager_bank.as_ref() {
                    let off = bank.live_offset_hz();
                    let live_hz = inspect_hz + off;
                    let mut s = status.lock().unwrap();
                    s.pager_sweep = Some(format!(
                        "channel {}/{} · {}{:.0} kHz",
                        bank.live_index() + 1,
                        bank.len(),
                        if off < 0.0 { "−" } else { "+" },
                        off.abs() / 1000.0
                    ));
                    s.pager_live_hz = Some(live_hz);
                    let _ = events.send(SdrEvent::Status(s.clone()));
                }
            }
        } else {
            pager_bank = None;
        }

        // AIS: both channels from the span, rebuilt when the span or the
        // tuner's center moves. Out of the span, nothing is decoded.
        if current_mode == SdrMode::Ais {
            let center = tuned_freq + lo_offset_for(current_rate, lo_offset, current_mode);
            let key = (current_rate, center);
            if ais_key != Some(key) {
                ais_key = Some(key);
                ais_rx = scannerd_engine::ais::AisReceiver::hears(center, current_rate)
                    .then(|| scannerd_engine::ais::AisReceiver::new(current_rate, scannerd_engine::ais::AIS_MID_HZ - center));
            }
        } else {
            ais_rx = None;
            ais_key = None;
        }

        // A spur inside the channel corrupts the decode, not just the picture.
        // The inspect chain has already mixed the channel to DC, so a spur at
        // display offset `s` lands at `s - channel_offset` here.
        //
        // Notches are cached per (offset, rate): retuning rebuilds them anyway,
        // and steady-state operation would otherwise redesign the NCOs and
        // one-pole coefficient every block for an unchanged spur geometry.
        if !spurs.is_empty() && !inspect_iq.is_empty() {
            let channel_off = inspect_hz - current_freq;
            let half_bw = f64::from(current_mode.bandwidth_hz()) / 2.0;
            let mut wanted: Vec<f64> = spurs
                .iter()
                .map(|&spur| spur - channel_off)
                .filter(|rel| rel.abs() < half_bw && rel.abs() > 100.0)
                .collect();
            wanted.sort_by(f64::total_cmp);
            if notch_cache.as_ref().map(|(key, _)| key != &wanted).unwrap_or(true) {
                notch_cache = Some((
                    wanted.clone(),
                    wanted
                        .iter()
                        .map(|&rel| ChannelNotch::new(rel, fs_chain, 4_000.0))
                        .collect::<Vec<_>>(),
                ));
            }
            if let Some((_, notches)) = &mut notch_cache {
                for notch in notches.iter_mut() {
                    notch.process(&mut inspect_iq, &mut notch_scratch);
                }
            }
        }

        audio_buf.clear();
        if let Demod::Wfm { stereo, .. } = &mut demod {
            stereo.clear();
        }

        // The classifier is the demodulator for every narrowband mode: it
        // already produces the voice and discriminator the decoders need, so
        // running it is not an extra cost. WFM has nothing it can use.
        //
        // The AFC runs ahead of it: the classifier's discriminator DC is the
        // residual carrier error, so fold that into the mix NCO before the
        // next block. Without this a signal 2 kHz off the clicked frequency
        // — ppm drift plus click imprecision — rides one skirt of the channel
        // filter and POCSAG/FLEX syncs get missed.
        let classification = match current_mode {
            SdrMode::Ais => {
                if let Some(rx) = ais_rx.as_mut() {
                    ais_frames.clear();
                    rx.process(&clean, &mut ais_frames);
                    // Each channel's health in the journal every 30 s: what
                    // tells "no ships in range" from "not hearing them".
                    if ais_logged.elapsed() >= Duration::from_secs(30) {
                        ais_logged = Instant::now();
                        let [a, b] = rx.levels();
                        eprintln!(
                            "AIS: A {} ok, {} bad crc, {} implausible, {:.1}/{:.1} dBFS mean/peak · B {} ok, {} bad crc, {} implausible, {:.1}/{:.1} dBFS",
                            a.good, a.bad, a.odd, a.mean_dbfs, a.peak_dbfs, b.good, b.bad, b.odd, b.mean_dbfs, b.peak_dbfs
                        );
                    }
                    for f in ais_frames.drain(..) {
                        let hz = if f.channel == 'A' { scannerd_engine::ais::AIS_A_HZ } else { scannerd_engine::ais::AIS_B_HZ };
                        decode_history.note(hz, "AIS");
                        let _ = events.send(SdrEvent::Decode { inspect_hz: hz, event: ais_event(&f) });
                    }
                }
                ClassificationResult::default()
            }
            SdrMode::Pager => {
                // Feed the span into the walking bank; emit decoded pages.
                if let Some(bank) = pager_bank.as_mut() {
                    let ms = block_ms;
                    let (flex_msgs, pocsag_msgs, live_audio) = bank.process(&clean, ms);
                    // Scope and audio monitor the LIVE channel (wired into the
                    // scope match below via pager_live_audio): FLEX's
                    // alternating 4-level pattern fills the trace on an active
                    // channel; dead channels show a noise floor. The level is
                    // what tells you where to centre.
                    audio_buf.extend_from_slice(&live_audio);
                    pager_live_audio.clear();
                    pager_live_audio.extend_from_slice(&live_audio);
                    // Labels belong on the channel that decoded, not on the
                    // tune frequency: a marker at the centre of the band
                    // claiming "FLEX" points the operator at empty air.
                    let live_hz = inspect_hz + bank.live_offset_hz();
                    for msg in flex_msgs {
                        decode_history.note(live_hz, "FLEX");
                        let _ = events.send(SdrEvent::Decode {
                            inspect_hz: live_hz,
                            event: flex_event(&msg),
                        });
                    }
                    for msg in pocsag_msgs {
                        decode_history.note(live_hz, "POCSAG");
                        let _ = events.send(SdrEvent::Decode {
                            inspect_hz: live_hz,
                            event: pocsag_event(&msg),
                        });
                    }
                }
                ClassificationResult::default()
            }
            SdrMode::Am | SdrMode::Wwv | SdrMode::Drm => {
                // No classifier: its measurements are FM-specific (deviation,
                // quieting, protocol syncs) and would read nonsense against an
                // envelope-modulated carrier. Demodulate straight to audio.
                if let Demod::Am {
                    demod: am,
                    audio,
                    hf,
                } = &mut demod
                {
                    // WWV's time code is read from the channel I/Q, stamped
                    // with the wall clock its seconds are measured against.
                    if let Some(hf) = hf {
                        hf.push_iq(&inspect_iq, inspect_hz, now_ms(), &events, &status);
                    }
                    am.set_agc(processing.agc);
                    am.process(&inspect_iq, audio);
                    // DRM: the decoded program, not the carrier's hiss.
                    if let Some(hf) = hf {
                        hf.take_audio(audio);
                    }
                    if processing.auto_notch {
                        mon_notch.process(audio);
                    }
                    mon_denoise.process(audio);
                    audio_buf.extend_from_slice(audio);
                    scope_src.clear();
                    scope_src.extend_from_slice(audio);
                }
                // The channel I/Q only: AM audio leaves at its own rate, and
                // the ring holds every kind at one rate (see CaptureBuffer).
                {
                    let empty: &[f32] = &[];
                    capture.lock().expect("SDR capture").append(inspect_hz, fs_chain, &inspect_iq, empty, empty);
                }
                ClassificationResult {
                    active: true,
                    modulation: "AM".into(),
                    ..Default::default()
                }
            }
            SdrMode::Usb | SdrMode::Lsb | SdrMode::Ft8 | SdrMode::Ft4 | SdrMode::Wefax | SdrMode::Rtty | SdrMode::Navtex | SdrMode::Psk31 | SdrMode::Sitor | SdrMode::Dsc | SdrMode::Hfdl | SdrMode::Pactor | SdrMode::Stanag | SdrMode::Ale | SdrMode::Cw | SdrMode::Js8 | SdrMode::Sstv | SdrMode::Wspr | SdrMode::Olivia | SdrMode::Mfsk | SdrMode::Hell => {
                // No classifier either: sideband has no carrier to measure
                // and no FM to discriminate. The demodulator picks the side
                // and runs its own speech AGC; the audio leaves at the
                // channel rate, so the rolling capture keeps it as voice.
                if let Demod::Ssb { demod: ssb, audio, linear, slots, hf } = &mut demod {
                    ssb.set_agc(processing.agc);
                    if let Some(hf) = hf {
                        // The text and picture decoders hear the audio before
                        // the AGC, as FT8's does.
                        ssb.process_with_linear(&inspect_iq, audio, linear);
                        hf.push(linear, inspect_hz, &events, &status);
                    } else {
                    match slots {
                        // FT8/FT4: the decoder hears the audio before the
                        // AGC and the speaker's processing, stamped with
                        // the wall clock the slots are cut by.
                        Some(slots) => {
                            ssb.process_with_linear(&inspect_iq, audio, linear);
                            slots.push(linear, now_ms());
                            for (slot_ms, decodes) in slots.take() {
                                // No peak labels: the history tags every peak
                                // near the noted frequency, and the dial is
                                // the only one FT8 has, so each signal in the
                                // 3 kHz above it was tagged and the tags
                                // came and went with the peaks.
                                for d in &decodes {
                                    let _ = events.send(SdrEvent::Decode { inspect_hz, event: ft8_event(slot_ms, d) });
                                }
                            }
                        }
                        None => ssb.process(&inspect_iq, audio),
                    }
                    }
                    if processing.auto_notch {
                        mon_notch.process(audio);
                    }
                    mon_denoise.process(audio);
                    audio_buf.extend_from_slice(audio);
                    scope_src.clear();
                    scope_src.extend_from_slice(audio);
                    let empty: &[f32] = &[];
                    capture
                        .lock()
                        .expect("SDR capture")
                        .append(inspect_hz, fs_chain, &inspect_iq, audio, empty);
                }
                ClassificationResult {
                    active: true,
                    modulation: match current_mode {
                        SdrMode::Lsb => "LSB",
                        SdrMode::Ft8 => "FT8",
                        SdrMode::Ft4 => "FT4",
                        SdrMode::Wefax => "WEFAX",
                        SdrMode::Rtty => "RTTY",
                        SdrMode::Navtex => "NAVTEX",
                        SdrMode::Psk31 => "PSK31",
                        SdrMode::Sitor => "SITOR",
                        SdrMode::Dsc => "DSC",
                        SdrMode::Hfdl => "HFDL",
                        SdrMode::Pactor => "PACTOR",
                        SdrMode::Stanag => "STANAG",
                        SdrMode::Ale => "ALE",
                        SdrMode::Cw => "CW",
                        SdrMode::Js8 => "JS8",
                        SdrMode::Sstv => "SSTV",
                        SdrMode::Wspr => "WSPR",
                        SdrMode::Olivia => "OLIVIA",
                        SdrMode::Mfsk => "MFSK",
                        SdrMode::Hell => "HELL",
                        _ => "USB",
                    }
                    .into(),
                    ..Default::default()
                }
            }
            SdrMode::Nfm | SdrMode::Packet | SdrMode::Flex => {
                if current_mode == SdrMode::Flex {
                    // Carrier offset tracking and steering are handled synchronously
                    // on sync locks in Demod::Flex to avoid single-sided idle frame distortion.
                } else if classifier.flex_is_active() {
                    // A FLEX frame is in flight on this channel (Packet and
                    // NFM both share it in Auto-style rigs): FLEX idle is
                    // single-sided, so the discriminator mean is not a
                    // carrier offset. Freeze the steering until it ends.
                    if afc.correction_hz().abs() >= 50.0 {
                        afc.clear_correction();
                        inspect_chain.set_offset(
                            inspect_hz - tuned_freq
                                - lo_offset_for(current_rate, lo_offset, current_mode),
                        );
                        afc_last_reported = 0.0;
                    }
                } else {
                    // PACKET steers exactly like NFM: a 2 kHz click error
                    // rides a skirt of the channel filter and the POCSAG/
                    // FLEX syncs are missed without it. Steer only on a
                    // locked, quieted carrier — the same lock the Auto
                    // branch below applies.
                    let locked = last_snr_db >= scannerd_engine::squelch::DEFAULT_OPEN_DB
                        && mon_gate.is_open();
                    afc.observe(last_center_offset_hz, locked);
                    let corr = afc.correction_hz();
                    if (corr - afc_last_reported).abs() >= 50.0 {
                        inspect_chain.set_offset(
                            inspect_hz - tuned_freq
                                - lo_offset_for(current_rate, lo_offset, current_mode)
                                + f64::from(corr),
                        );
                        afc_last_reported = corr;
                    }
                }
                let c = classifier.process(&inspect_iq, inspect_hz);
                last_center_offset_hz = c.center_offset_hz;
                // Weak-signal listening: gate static, level what survives.
                if current_mode != SdrMode::Flex {
                    let tail = audio_buf.len();
                    audio_buf.extend_from_slice(classifier.monitor_voice());
                    let voice = &mut audio_buf[tail..];
                    if processing.auto_notch {
                        mon_notch.process(voice);
                    }
                    mon_denoise.process(voice);
                    mon_gate.process(voice, classifier.monitor_noise());
                    mon_leveler.process(voice);
                }
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
            SdrMode::Scan => {
                // The slot rigs run every block so their receivers stay warm,
                // but the audio path only carries a channel that has earned a
                // lock: while sweeping and testing, LISTEN stays quiet instead
                // of churning through every candidate's first syllable.
                //
                // The shared inspect chain sits wherever the last tune or
                // lock left it; its only scan duty is the diagnostic capture.
                {
                    let mut capture = capture.lock().expect("SDR capture");
                    let empty: &[f32] = &[];
                    capture.append(inspect_hz, fs_chain, &inspect_iq, empty, empty);
                }

                // Gather one block of evidence per running slot. Every
                // receiver — digital and analog — reads the slot's one
                // narrowband chain output, so a slot costs a single
                // mix-and-filter pass over the span per block.
                scan_signals.clear();
                scan_signals.resize(scan_cfg.slots, None);
                scan_recs.resize_with(scan_cfg.slots, CallRecorder::new);
                scan_rec_meta.resize_with(scan_cfg.slots, || None);
                let mut slot_events: Vec<(usize, f64, ScanMode, CallEvent)> = Vec::new();
                for (i, slot_opt) in scan_slots.iter_mut().enumerate() {
                    let Some(slot) = slot_opt else { continue };
                    slot.chain.process(&clean, &mut slot.chain_iq);

                    let mut control_pulse = false;
                    let mut p25_ev = None;
                    let mut dmr_ev = None;
                    let (mut p25_locked, mut p25_in_call) = (false, false);
                    let (mut dmr_locked, mut dmr_in_call) = (false, false);
                    if let Some(rx) = slot.p25.as_mut() {
                        p25_ev = rx.process(&slot.chain_iq, slot.snr_db);
                        for grant in rx.pending_grants() {
                            decode_history.note(slot.freq_hz, "P25");
                            control_pulse = true;
                            let _ = events.send(SdrEvent::Decode {
                                inspect_hz: slot.freq_hz,
                                event: grant_event("P25", &grant_repr(&grant)),
                            });
                        }
                        (p25_locked, p25_in_call) = (rx.locked(), rx.in_call());
                    }
                    if let Some(rx) = slot.dmr.as_mut() {
                        dmr_ev = rx.process(&slot.chain_iq, slot.snr_db);
                        // Data frames while synced and never in a call is
                        // what a control channel sounds like.
                        control_pulse |= !rx.decoded_data().is_empty();
                        (dmr_locked, dmr_in_call) = (rx.locked(), rx.in_call());
                    }
                    if let Some(e) = p25_ev {
                        slot_events.push((i, slot.freq_hz, ScanMode::P25, e));
                    }
                    if let Some(e) = dmr_ev {
                        slot_events.push((i, slot.freq_hz, ScanMode::Dmr, e));
                    }

                    // Whichever receiver is carrying a call is the evidence;
                    // when none is, lock state still says whether something
                    // digital has the frequency.
                    let digital = if p25_in_call {
                        Some(DigitalEvidence {
                            mode: ScanMode::P25,
                            locked: true,
                            in_call: true,
                            control: control_pulse,
                        })
                    } else if dmr_in_call {
                        Some(DigitalEvidence {
                            mode: ScanMode::Dmr,
                            locked: true,
                            in_call: true,
                            control: control_pulse,
                        })
                    } else if p25_locked {
                        Some(DigitalEvidence {
                            mode: ScanMode::P25,
                            locked: true,
                            in_call: false,
                            control: control_pulse,
                        })
                    } else if dmr_locked {
                        Some(DigitalEvidence {
                            mode: ScanMode::Dmr,
                            locked: true,
                            in_call: false,
                            control: control_pulse,
                        })
                    } else {
                        None
                    };

                    let mut am_rms = None;
                    if let Some(am_demod) = slot.am.as_mut() {
                        am_demod.process(&slot.chain_iq, &mut slot.am_audio);
                        am_rms = Some(rms(&slot.am_audio));
                    }

                    // NFM voice evidence: the same gated monitor chain the
                    // listening modes use, per slot, kept for routing if this
                    // slot earns the lock.
                    let mut nfm_rms = None;
                    if let Some(fm) = slot.nbfm.as_mut() {
                        fm.process(&slot.chain_iq, &mut slot.fm_out);
                        let mut voice = std::mem::take(&mut slot.nfm_voice);
                        voice.clear();
                        voice.extend_from_slice(&slot.fm_out.voice);
                        slot.mon_notch.process(&mut voice);
                        slot.mon_gate.process(&mut voice, &slot.fm_out.noise);
                        slot.mon_leveler.process(&mut voice);
                        nfm_rms = Some(rms(&voice));
                        slot.nfm_voice = voice;
                    }

                    scan_signals[i] = Some(ScanSignal {
                        snr_db: slot.snr_db,
                        digital,
                        nfm_voice_rms: nfm_rms,
                        am_voice_rms: am_rms,
                    });
                }

                // Evidence is gathered first, the scanner judges it, and only
                // then is audio routed — the judge may lock (or release) this
                // very block.
                if let Some(sc) = scanner.as_mut() {
                    let acts = sc.note_signals(
                        scan_t0.elapsed().as_secs_f64(),
                        &scan_signals,
                        block_ms as f32 / 1000.0,
                    );
                    scan_actions.extend(acts);
                }

                // Route audio for a lock; silence otherwise. Whatever is
                // routed also names its sample rate — see scan_audio_rate.
                // The speaker carries the newest call still held; every
                // other held call records without disturbing it.
                let listen = scanner.as_ref().and_then(|sc| sc.listen());
                let lock_mode = listen.map(|(_, m)| m);
                let lock_slot = listen.map(|(i, _)| i);
                match lock_mode {
                    Some(ScanMode::P25) => {
                        if let Some(Some(slot)) = lock_slot.and_then(|i| scan_slots.get_mut(i))
                            && let Some(rx) = slot.p25.as_ref()
                        {
                            audio_buf.extend_from_slice(rx.audio());
                            scan_audio_rate = rx.audio_rate();
                        }
                    }
                    Some(ScanMode::Dmr) => {
                        if let Some(Some(slot)) = lock_slot.and_then(|i| scan_slots.get_mut(i))
                            && let Some(rx) = slot.dmr.as_ref()
                        {
                            audio_buf.extend_from_slice(rx.audio());
                            scan_audio_rate = rx.audio_rate();
                        }
                    }
                    Some(ScanMode::Nfm) => {
                        if let Some(Some(slot)) = lock_slot.and_then(|i| scan_slots.get(i)) {
                            audio_buf.extend_from_slice(&slot.nfm_voice);
                            scan_audio_rate = fs_chain;
                        }
                    }
                    Some(ScanMode::Am) => {
                        if let Some(Some(slot)) = lock_slot.and_then(|i| scan_slots.get(i)) {
                            audio_buf.extend_from_slice(&slot.am_audio);
                            scan_audio_rate = fs_chain;
                        }
                    }
                    None => {}
                }

                // The shared inspect chain — and with it the capture buffer
                // and the passband marker — follows the locked channel.
                if let (Some(m), Some(i)) = (lock_mode, lock_slot) {
                    if scan_nudged_for != Some(i)
                        && let Some(Some(slot)) = scan_slots.get(i)
                    {
                        inspect_hz = slot.freq_hz;
                        inspect_chain.set_offset(
                            inspect_hz - tuned_freq
                                - lo_offset_for(current_rate, lo_offset, current_mode),
                        );
                        scan_nudged_for = Some(i);
                        let mut s = status.lock().unwrap();
                        s.inspect_hz = inspect_hz;
                        let _ = events.send(SdrEvent::Status(s.clone()));
                    }

                    // Re-centre the receiver on its own carrier measurement.
                    // The ppm residual can park a candidate a kilohertz or two
                    // wide of the true carrier — the display shows the passband
                    // sitting beside the signal, and the receivers' own
                    // tracking only reaches ±2.5 kHz. The receivers ride the
                    // slot's chain, so shifting the chain's reference (not a
                    // rebuild) keeps the call running and puts the passband
                    // where the signal is; the receivers' frequency bookkeeping
                    // follows for the log.
                    if let Some(Some(slot)) = scan_slots.get_mut(i) {
                        let measured = match (m, slot.p25.as_ref(), slot.dmr.as_ref()) {
                            (ScanMode::P25, Some(rx), _) => rx.carrier_offset_hz(),
                            (ScanMode::Dmr, _, Some(rx)) => rx.carrier_offset_hz(),
                            _ => None,
                        };
                        if let Some(off) = measured
                            .filter(|o| o.abs() >= 800.0)
                            .filter(|_| scan_fine_nudged_for != Some(i))
                        {
                            // The confirmed LO is the anchor the chain was
                            // last mixed against; an optimistically requested
                            // tune would re-offset it.
                            let span_center = tuned_freq
                                + lo_offset_for(current_rate, lo_offset, current_mode);
                            if let Some(rx) = slot.p25.as_mut() {
                                rx.spec.freq_hz += off;
                            }
                            if let Some(rx) = slot.dmr.as_mut() {
                                rx.spec.freq_hz += off;
                            }
                            slot.freq_hz += off;
                            slot.chain.set_offset(slot.freq_hz - span_center);
                            inspect_hz += off;
                            inspect_chain.set_offset(
                                inspect_hz - tuned_freq
                                    - lo_offset_for(current_rate, lo_offset, current_mode),
                            );
                            scan_nudged_for = Some(i);
                            scan_fine_nudged_for = Some(i);
                            let mut s = status.lock().unwrap();
                            s.inspect_hz = inspect_hz;
                            let _ = events.send(SdrEvent::Status(s.clone()));
                        }
                    }
                } else {
                    scan_nudged_for = None;
                    scan_fine_nudged_for = None;
                }

                // Whatever the receivers decoded goes in the log whether or
                // not the scanner kept the channel: a call that started and
                // ended inside a test dwell is still worth having seen.
                for (_i, f, ev_mode, ev) in &slot_events {
                    let sdr_mode = if *ev_mode == ScanMode::P25 {
                        SdrMode::P25
                    } else {
                        SdrMode::Dmr
                    };
                    let _ = events.send(SdrEvent::Decode {
                        inspect_hz: *f,
                        event: call_event(sdr_mode, ev),
                    });
                }
                // Recording. Every slot in a call records its own audio —
                // digital bracketed by its receiver's call events, analog
                // following that slot's squelch with the hang trimmed off
                // the tail — while the speaker carries only the newest
                // call. One speaker, many recorders.
                let rec_states = scanner.as_ref().map(|sc| sc.recording_states());
                for i in 0..scan_recs.len() {
                    // This slot's own call events bracket its recorder.
                    for (si, f, ev_mode, ev) in &slot_events {
                        if *si != i {
                            continue;
                        }
                        match ev {
                            CallEvent::Started => {
                                // The rate of the audio that will be
                                // recorded — the receiver's own, NOT
                                // demod.audio_rate(), which has already
                                // forgotten the call by the time Ended
                                // fires.
                                let rate = scan_slots
                                    .get(i)
                                    .and_then(|s| s.as_ref())
                                    .and_then(|slot| match ev_mode {
                                        ScanMode::P25 => {
                                            slot.p25.as_ref().map(|rx| rx.audio_rate())
                                        }
                                        ScanMode::Dmr => {
                                            slot.dmr.as_ref().map(|rx| rx.audio_rate())
                                        }
                                        _ => None,
                                    })
                                    .unwrap_or(fs_chain);
                                scan_recs[i].start(rate, fs_chain, now_ms());
                                scan_rec_meta[i] = Some((*ev_mode, rate.round().max(1.0) as u32));
                            }
                            CallEvent::Ended(summary) => {
                                if !scan_recs[i].active {
                                    continue;
                                }
                                if let Some(Some(slot)) = scan_slots.get(i) {
                                    scan_recs[i].push_iq(&slot.chain_iq);
                                }
                                let (buf, iq, iq_rate_hz, started) = scan_recs[i].stop();
                                let digital = summary.digital.as_ref();
                                file_call(
                                    &calls,
                                    RecordedCall {
                                        id: next_call_id,
                                        protocol: ev_mode.protocol().into(),
                                        started_ms: started,
                                        duration_s: summary.duration_s(),
                                        freq_hz: *f,
                                        rate_hz: scan_rec_meta[i]
                                            .map(|(_, r)| r)
                                            .unwrap_or(8_000),
                                        samples: buf.len(),
                                        iq_rate_hz,
                                        iq_samples: iq.len() / 2,
                                        peak: call_peak(&buf),
                                        encrypted: digital.is_some_and(|d| d.encrypted),
                                        decrypted: digital.is_some_and(|d| d.decrypted),
                                        peak_snr_db: summary
                                            .peak_snr_db
                                            .is_finite()
                                            .then_some(summary.peak_snr_db),
                                        freq_error_hz: summary
                                            .freq_error_hz
                                            .is_finite()
                                            .then_some(summary.freq_error_hz),
                                        voiced_fraction: summary
                                            .voiced_fraction
                                            .is_finite()
                                            .then_some(summary.voiced_fraction),
                                        algorithm: digital
                                            .and_then(|d| d.algorithm_id)
                                            .map(|a| format!("0x{a:02X}")),
                                        key_id: digital.and_then(|d| d.key_id),
                                        tone: summary.tone.as_ref().map(|t| t.label()),
                                        mdc: summary.mdc.as_ref().map(|m| m.label()),
                                        digital_protocol: digital.map(|d| d.protocol.clone()),
                                        color_code: digital.and_then(|d| d.color_code),
                                        slot: digital.and_then(|d| d.slot),
                                        source_id: digital.and_then(|d| d.source_id),
                                        target_id: digital.and_then(|d| d.target_id),
                                        group: digital.and_then(|d| d.group),
                                        manufacturer: digital.and_then(|d| d.manufacturer.clone()),
                                        service_options: digital.and_then(|d| d.service_options),
                                        emergency: digital.is_some_and(|d| d.emergency),
                                        talker_alias: digital
                                            .and_then(|d| d.talker_alias.clone()),
                                        error_pct: digital.and_then(|d| d.bit_error_pct),
                                        audio: buf,
                                        iq,
                                    },
                                );
                                next_call_id += 1;
                            }
                        }
                    }
                    let state = rec_states
                        .as_ref()
                        .and_then(|r| r.get(i).copied().flatten());
                    match state {
                        Some(rs) if matches!(rs.mode, ScanMode::P25 | ScanMode::Dmr) => {
                            // In a digital call: record this slot's own
                            // receiver audio for as long as it lasts.
                            if scan_recs[i].active
                                && let Some(Some(slot)) = scan_slots.get(i)
                            {
                                scan_recs[i].push_iq(&slot.chain_iq);
                                let audio = match rs.mode {
                                    ScanMode::P25 => slot.p25.as_ref().map(|rx| rx.audio()),
                                    ScanMode::Dmr => slot.dmr.as_ref().map(|rx| rx.audio()),
                                    _ => None,
                                };
                                if let Some(audio) = audio {
                                    scan_recs[i].push(audio);
                                }
                            }
                        }
                        Some(rs) => {
                            // Analog: the slot's squelch brackets the
                            // recording, with the hang trimmed off the tail
                            // so it ends where the talking did.
                                if rs.analog_open {
                                    if !scan_recs[i].active {
                                        scan_recs[i].start(fs_chain, fs_chain, now_ms());
                                        scan_rec_meta[i] =
                                            Some((rs.mode, (fs_chain as u32).max(1)));
                                    }
                                if let Some(Some(slot)) = scan_slots.get(i) {
                                    scan_recs[i].push_iq(&slot.chain_iq);
                                    match rs.mode {
                                        ScanMode::Nfm => scan_recs[i].push(&slot.nfm_voice),
                                        ScanMode::Am => scan_recs[i].push(&slot.am_audio),
                                        _ => {}
                                    }
                                }
                            } else if scan_recs[i].active {
                                let (buf, mut iq, iq_rate_hz, started) = scan_recs[i].stop();
                                let rate = scan_rec_meta[i]
                                    .map(|(_, r)| r)
                                    .unwrap_or(fs_chain as u32)
                                    .max(1);
                                let trim = (rs.trailing_s * rate as f32).round() as usize;
                                let kept = &buf[..buf.len().saturating_sub(trim)];
                                let iq_trim =
                                    (rs.trailing_s * iq_rate_hz as f32).round() as usize * 2;
                                iq.truncate(iq.len().saturating_sub(iq_trim));
                                let peak = call_peak(kept);
                                let duration = kept.len() as f32 / rate as f32;
                                // A blip of squelch with no voice behind it
                                // is not a call; Last Heard has no room for
                                // filed silence.
                                if peak >= 0.002 && duration >= 0.4 {
                                    let freq = scan_slots
                                        .get(i)
                                        .and_then(|s| s.as_ref())
                                        .map(|s| s.freq_hz)
                                        .unwrap_or(inspect_hz);
                                    let snr_db = scan_slots
                                        .get(i)
                                        .and_then(|s| s.as_ref())
                                        .map(|s| s.snr_db)
                                        .filter(|v| v.is_finite());
                                    file_call(
                                        &calls,
                                        RecordedCall {
                                            id: next_call_id,
                                            protocol: rs.mode.protocol().into(),
                                            started_ms: started,
                                            duration_s: duration,
                                            freq_hz: freq,
                                            rate_hz: rate,
                                            samples: kept.len(),
                                            iq_rate_hz,
                                            iq_samples: iq.len() / 2,
                                            peak,
                                            encrypted: false,
                                            decrypted: false,
                                            peak_snr_db: snr_db,
                                            freq_error_hz: None,
                                            voiced_fraction: None,
                                            algorithm: None,
                                            key_id: None,
                                            tone: None,
                                            mdc: None,
                                            digital_protocol: None,
                                            color_code: None,
                                            slot: None,
                                            source_id: None,
                                            target_id: None,
                                            group: None,
                                            manufacturer: None,
                                            service_options: None,
                                            emergency: false,
                                            talker_alias: None,
                                            error_pct: None,
                                            audio: kept.to_vec(),
                                            iq,
                                        },
                                    );
                                    next_call_id += 1;
                                }
                            }
                        }
                        // The slot left its call without a proper close —
                        // released, reseated or skipped mid-call: file what
                        // it caught, or drop silence.
                        None if scan_recs[i].active => {
                            let freq = scan_slots
                                .get(i)
                                .and_then(|s| s.as_ref())
                                .map(|s| s.freq_hz)
                                .unwrap_or(inspect_hz);
                            flush_scan_rec(
                                &mut scan_recs[i],
                                scan_rec_meta[i],
                                freq,
                                (fs_chain as u32).max(1),
                                &calls,
                                &mut next_call_id,
                            );
                        }
                        None => {}
                    }
                }

                // A lock knows what it is; the sweep reports itself instead.
                // The SNR and level fields arrive from the spectrum in the
                // frame pass, as in every mode that skips the classifier.
                match lock_mode {
                    Some(ScanMode::P25) => ClassificationResult {
                        active: true,
                        protocol: "P25 Phase 1".into(),
                        modulation: "C4FM @ 4800 Bd".into(),
                        details: Some("voice call".into()),
                        confidence: 0.95,
                        ..ClassificationResult::default()
                    },
                    Some(ScanMode::Dmr) => ClassificationResult {
                        active: true,
                        protocol: "DMR Tier II".into(),
                        modulation: "4-FSK @ 4800 Bd".into(),
                        details: Some("voice call".into()),
                        confidence: 0.95,
                        ..ClassificationResult::default()
                    },
                    Some(ScanMode::Nfm) => ClassificationResult {
                        active: true,
                        protocol: "Analog FM".into(),
                        modulation: "FM".into(),
                        details: Some("voice".into()),
                        confidence: 0.8,
                        ..ClassificationResult::default()
                    },
                    Some(ScanMode::Am) => ClassificationResult {
                        active: true,
                        protocol: "AM voice".into(),
                        modulation: "AM".into(),
                        details: Some("voice".into()),
                        confidence: 0.8,
                        ..ClassificationResult::default()
                    },
                    None => {
                        let where_at = scanner
                            .as_ref()
                            .map(|sc| sc.progress_line())
                            .unwrap_or_else(|| "scan".into());
                        ClassificationResult {
                            active: false,
                            protocol: "scan".into(),
                            modulation: "—".into(),
                            details: Some(where_at),
                            confidence: 0.0,
                            ..ClassificationResult::default()
                        }
                    }
                }
            }
            SdrMode::Auto => {
                // AUTO steers the same discriminator the packet decoders read:
                // an unsteered carrier costs it POCSAG/FLEX syncs exactly as
                // it does PACKET. Freeze that steering while FLEX is synced —
                // idle FLEX is single-sided and the disc mean is not a carrier
                // offset.
                if classifier.flex_is_active() {
                    if afc.correction_hz().abs() >= 50.0 {
                        afc.clear_correction();
                        inspect_chain.set_offset(
                            inspect_hz - tuned_freq
                                - lo_offset_for(current_rate, lo_offset, current_mode),
                        );
                        afc_last_reported = 0.0;
                    }
                } else {
                    // Steer only on a locked, quieted carrier: an unlocked
                    // discriminator mean is noise with a random DC, and
                    // integrating it walks the NCO off the channel. The
                    // 8 dB open threshold (not the old 2.5 dB) is the same
                    // bar the squelch applies before believing a signal.
                    let locked = last_snr_db >= scannerd_engine::squelch::DEFAULT_OPEN_DB
                        && mon_gate.is_open();
                    afc.observe(last_center_offset_hz, locked);
                    let corr = afc.correction_hz();
                    if (corr - afc_last_reported).abs() >= 50.0 {
                        inspect_chain.set_offset(
                            inspect_hz - tuned_freq
                                - lo_offset_for(current_rate, lo_offset, current_mode)
                                + f64::from(corr),
                        );
                        afc_last_reported = corr;
                    }
                }
                // Run everything. The classifier identifies and supplies the
                // discriminator the packet decoders read; the two voice
                // receivers work the same channel in parallel, and whichever
                // one locks is what gets believed and heard.
                let c = classifier.process(&inspect_iq, inspect_hz);
                last_center_offset_hz = c.center_offset_hz;
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
                    if let Some(ev) = p25.process(&inspect_iq, last_snr_db) {
                        let _ = events.send(SdrEvent::Decode {
                            inspect_hz,
                            event: call_event(SdrMode::P25, &ev),
                        });
                    }
                    for grant in p25.pending_grants() {
                        decode_history.note(inspect_hz, "P25");
                        let _ = events.send(SdrEvent::Decode {
                            inspect_hz,
                            event: grant_event("P25", &grant_repr(&grant)),
                        });
                    }
                    if let Some(ev) = dmr.process(&inspect_iq, last_snr_db) {
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
                        let tail = audio_buf.len();
                        audio_buf.extend_from_slice(classifier.monitor_voice());
                        let voice = &mut audio_buf[tail..];
                        // No noise reduction here: the audio switches to
                        // decoded voice mid-stream, and the reducer's frame of
                        // analog audio would play out over the start of it.
                        if processing.auto_notch {
                            mon_notch.process(voice);
                        }
                        mon_gate.process(voice, classifier.monitor_noise());
                        mon_leveler.process(voice);
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
                    let mut aprs_heard = false;
                    for packet in aprs.process(classifier.standard_discriminator_hz()) {
                        aprs_heard = true;
                        decode_history.note(inspect_hz, "APRS");
                        let _ = events.send(SdrEvent::Decode {
                            inspect_hz,
                            event: packet_event(&packet),
                        });
                    }
                    // The demod owns the pager banks here — the classifier's
                    // own were dropped (use_external_pagers), so these decoders
                    // are the only ones slicing the discriminator. Syncs are
                    // fed back so the classifier's protocol matching still sees
                    // them.
                    let mut pocsag_sync_baud = None;
                    let mut flex_sync_baud = None;
                    for (baud, dec) in [512u32, 1200, 2400].into_iter().zip(pocsag.iter_mut()) {
                        let syncs_before = {
                            let d = dec.diagnostics();
                            d.syncs_512 + d.syncs_1200 + d.syncs_2400
                        };
                        for msg in dec.process(disc) {
                            decode_history.note(inspect_hz, "POCSAG");
                            let _ = events.send(SdrEvent::Decode {
                                inspect_hz,
                                event: pocsag_event(&msg),
                            });
                        }
                        let d = dec.diagnostics();
                        let syncs_after = d.syncs_512 + d.syncs_1200 + d.syncs_2400;
                        if syncs_after > syncs_before {
                            pocsag_sync_baud =
                                Some(d.last_sync_baud.unwrap_or(baud));
                        }
                    }
                    let flex_syncs_before = {
                        let d = flex.diagnostics();
                        (d.syncs_1600, d.syncs_3200, d.syncs_6400)
                    };
                    let mut flex_pages = 0usize;
                    for msg in flex.process(disc) {
                        decode_history.note(inspect_hz, "FLEX");
                        let _ = events.send(SdrEvent::Decode {
                            inspect_hz,
                            event: flex_event(&msg),
                        });
                        flex_pages += 1;
                    }
                    // Page-time capture: dump the raw span ring and the exact
                    // discriminator the decoders just consumed, so any page can
                    // be replayed offline against reference decoders. Opt-in.
                    static PAGE_DUMP: AtomicUsize = AtomicUsize::new(0);
                    if flex_pages > 0 && std::env::var_os("USDR_PAGE_DUMP").is_some() {
                        let n = PAGE_DUMP.fetch_add(1, Ordering::Relaxed);
                        let (span_wav, disc_wav) = {
                            let (span, span_rate, disc, disc_rate) =
                                capture.lock().expect("SDR capture").page_debug_samples();
                            // Encode outside the lock — see `page_debug_samples`.
                            (
                                pcm_wav(&span, span_rate, 2),
                                pcm_wav(&disc, disc_rate, 1),
                            )
                        };
                        let span_path = format!("/tmp/usdr_page{n}_span.wav");
                        let disc_path = format!("/tmp/usdr_page{n}_disc.wav");
                        let _ = std::fs::write(&span_path, span_wav);
                        let _ = std::fs::write(&disc_path, disc_wav);
                        eprintln!(
                            "SDR: dumped {span_path} and {disc_path} after {flex_pages} page(s)"
                        );
                    }
                    {
                        let d = flex.diagnostics();
                        if d.syncs_6400 > flex_syncs_before.2 {
                            flex_sync_baud = Some(6400);
                        } else if d.syncs_3200 > flex_syncs_before.1 {
                            flex_sync_baud = Some(3200);
                        } else if d.syncs_1600 > flex_syncs_before.0 {
                            flex_sync_baud = Some(1600);
                        }
                    }
                    if let Some(baud) = pocsag_sync_baud {
                        classifier.note_pocsag_sync(baud);
                    }
                    if let Some(baud) = flex_sync_baud {
                        classifier.note_flex_sync(baud);
                    }
                    if aprs_heard {
                        classifier.note_aprs_frame();
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
            SdrMode::P25 | SdrMode::Dmr | SdrMode::Nxdn => {
                // These receivers extract and equalise their own channel out
                // of the span, so they are fed the corrected span rather than
                // the inspect chain's narrowband output.
                // The receivers use this only for the call summary, but a
                // real figure is more use in the log than a hardcoded zero.
                let snr_hint = last_snr_db;
                let (event, locked, in_call) = match &mut demod {
                    Demod::P25 { rx } => {
                        let ev = rx.process(&inspect_iq, snr_hint);
                        audio_buf.extend_from_slice(rx.audio());
                        for grant in rx.pending_grants() {
                            decode_history.note(inspect_hz, "P25");
                            let _ = events.send(SdrEvent::Decode {
                                inspect_hz,
                                event: grant_event("P25", &grant_repr(&grant)),
                            });
                        }
                        (ev, rx.locked(), rx.in_call())
                    }
                    Demod::Dmr { rx } => {
                        let ev = rx.process(&inspect_iq, snr_hint);
                        audio_buf.extend_from_slice(rx.audio());
                        (ev, rx.locked(), rx.in_call())
                    }
                    Demod::Nxdn { rx } => {
                        let ev = rx.process(&inspect_iq, snr_hint);
                        audio_buf.extend_from_slice(rx.audio());
                        // Type-C/Type-D control channels announce voice-call
                        // assignments: surface them like P25 trunk grants.
                        for grant in rx.pending_grants() {
                            decode_history.note(inspect_hz, "NXDN");
                            let _ = events.send(SdrEvent::Decode {
                                inspect_hz,
                                event: grant_event("NXDN", &nxdn_grant_repr(&grant)),
                            });
                        }
                        (ev, rx.locked(), rx.in_call())
                    }
                    _ => (None, false, false),
                };
                // The rolling capture keeps this channel's I/Q, so a call
                // that went past is in the recorder as well as in Last Heard.
                // Voice is left out: the vocoders run at 8 kHz, not the
                // ring's channel rate.
                {
                    let empty: &[f32] = &[];
                    capture.lock().expect("SDR capture").append(inspect_hz, fs_chain, &inspect_iq, empty, empty);
                }

                // Keep the call's audio so it can be replayed. Started arrives
                // on the same pass that produced the first audio, so the reset
                // has to happen before this pass is appended.
                if matches!(event, Some(CallEvent::Started)) {
                    let rec_rate = match &demod {
                        Demod::P25 { rx } => rx.audio_rate(),
                        Demod::Dmr { rx } => rx.audio_rate(),
                        Demod::Nxdn { rx } => rx.audio_rate(),
                        _ => fs_chain,
                    };
                    rec_voice.start(rec_rate, fs_chain, now_ms());
                }
                if rec_voice.active {
                    rec_voice.push(&audio_buf);
                    rec_voice.push_iq(&inspect_iq);
                }
                if let Some(CallEvent::Ended(summary)) = &event
                    && rec_voice.active
                {
                    let (buf, iq, iq_rate_hz, started) = rec_voice.stop();
                    let digital = summary.digital.as_ref();
                    // Same scaling `call_peak` documents: recorder stores
                    // full-scale-fraction * 28 000, so / 28 000 here too.
                    let peak = buf
                        .iter()
                        .map(|&v| (v as f32 / 28_000.0).abs())
                        .fold(0.0f32, f32::max);
                    let record = RecordedCall {
                        id: next_call_id,
                        protocol: match current_mode {
                            SdrMode::P25 => "P25",
                            SdrMode::Nxdn => "NXDN",
                            _ => "DMR",
                        }
                        .into(),
                        started_ms: started,
                        duration_s: summary.duration_s(),
                        freq_hz: inspect_hz,
                        rate_hz: demod.audio_rate(fs_chain).round() as u32,
                        samples: buf.len(),
                        iq_rate_hz,
                        iq_samples: iq.len() / 2,
                        peak,
                        encrypted: digital.is_some_and(|d| d.encrypted),
                        decrypted: digital.is_some_and(|d| d.decrypted),
                        peak_snr_db: summary
                            .peak_snr_db
                            .is_finite()
                            .then_some(summary.peak_snr_db),
                        freq_error_hz: summary
                            .freq_error_hz
                            .is_finite()
                            .then_some(summary.freq_error_hz),
                        voiced_fraction: summary
                            .voiced_fraction
                            .is_finite()
                            .then_some(summary.voiced_fraction),
                        algorithm: digital
                            .and_then(|d| d.algorithm_id)
                            .map(|a| format!("0x{a:02X}")),
                        key_id: digital.and_then(|d| d.key_id),
                        tone: summary.tone.as_ref().map(|t| t.label()),
                        mdc: summary.mdc.as_ref().map(|m| m.label()),
                        digital_protocol: digital.map(|d| d.protocol.clone()),
                        color_code: digital.and_then(|d| d.color_code),
                        slot: digital.and_then(|d| d.slot),
                        source_id: digital.and_then(|d| d.source_id),
                        target_id: digital.and_then(|d| d.target_id),
                        group: digital.and_then(|d| d.group),
                        manufacturer: digital.and_then(|d| d.manufacturer.clone()),
                        service_options: digital.and_then(|d| d.service_options),
                        emergency: digital.is_some_and(|d| d.emergency),
                        talker_alias: digital.and_then(|d| d.talker_alias.clone()),
                        error_pct: digital.and_then(|d| d.bit_error_pct),
                        audio: buf,
                        iq,
                    };
                    next_call_id += 1;
                    file_call(&calls, record);
                }
                if let Some(ev) = event {
                    let _ = events.send(SdrEvent::Decode {
                        inspect_hz,
                        event: call_event(current_mode, &ev),
                    });
                }
                ClassificationResult {
                    active: in_call,
                    protocol: match current_mode {
                        SdrMode::P25 => "P25 Phase 1".into(),
                        SdrMode::Nxdn => match &demod {
                            Demod::Nxdn { rx } => rx.rate_label().unwrap_or("NXDN").into(),
                            _ => "NXDN".into(),
                        },
                        _ => "DMR Tier II".into(),
                    },
                    modulation: match current_mode {
                        SdrMode::P25 => "C4FM @ 4800 Bd".into(),
                        SdrMode::Nxdn => "C4FM @ 2400/4800 Bd".into(),
                        _ => "4-FSK @ 4800 Bd".into(),
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
                    audio,
                    stereo,
                    pilot_seen,
                    rds,
                    rds_seen,
                    rds_pushed,
                    eq,
                    eq_iq,
                } = &mut demod
                {
                    // Multipath undone first, where the equalizer finds any:
                    // reflections ripple FM's flat envelope with the program,
                    // and the discriminator turns that into distortion worst
                    // in the stereo difference.
                    let iq: &[num_complex::Complex32] = match eq {
                        Some(eq) => {
                            eq_iq.clear();
                            eq_iq.extend_from_slice(&inspect_iq);
                            eq.process(eq_iq);
                            eq_iq
                        }
                        None => &inspect_iq,
                    };
                    // Instantaneous frequency in Hz, then the broadcast audio:
                    // mono always, stereo too while a pilot is locked.
                    fm.discriminator_hz(iq, disc);
                    // USDR_STEREO_TRACE=1: the channel's envelope ripple
                    // beside the stereo decoder's readings. FM's envelope is
                    // flat; noise ripples it a little, multipath a lot.
                    static TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                    if *TRACE.get_or_init(|| std::env::var_os("USDR_STEREO_TRACE").is_some()) {
                        static ENV: std::sync::Mutex<(f64, f64, usize)> = std::sync::Mutex::new((0.0, 0.0, 0));
                        let mut e = ENV.lock().unwrap();
                        for z in &inspect_iq {
                            let a = z.norm() as f64;
                            e.0 += a;
                            e.1 += a * a;
                            e.2 += 1;
                        }
                        if e.2 >= 512_000 {
                            let n = e.2 as f64;
                            let mean = e.0 / n;
                            let sd = (e.1 / n - mean * mean).max(0.0).sqrt();
                            eprintln!("ENVELOPE ripple {:.2}% of the mean over {:.1} s", 100.0 * sd / mean.max(1e-12), n / fs_chain);
                            *e = (0.0, 0.0, 0);
                        }
                    }
                    audio.process(disc, &mut audio_buf, stereo);
                    let st = audio.status();
                    if st.pilot != *pilot_seen {
                        *pilot_seen = st.pilot;
                        if let Ok(mut s) = status.lock() {
                            s.stereo = Some(st);
                            let _ = events.send(SdrEvent::Status(s.clone()));
                        }
                    }
                    // Deliberately not captured. The rolling buffer holds
                    // I/Q, discriminator and audio at one rate, and wideband
                    // FM has the first two at 256 kHz and the audio at 51.2 —
                    // storing them together would label the voice WAV at five
                    // times its real rate. The I/Q would be useless for replay
                    // regardless: 256 kHz is outside the range the classifier
                    // accepts, and none of the digital decoders apply here.

                    // RDS reads the multiplex before de-emphasis, which is
                    // where the 57 kHz subcarrier still is.
                    rds.process(disc);
                    for line in rds.take_trace() {
                        eprintln!("RDS {line}");
                    }
                    for change in rds.take_changes() {
                        if let Some(event) = rds.event(&change) {
                            decode_history.note(inspect_hz, "RDS");
                            let _ = events.send(SdrEvent::Decode { inspect_hz, event });
                        }
                    }
                    // The station's state rides in the status: pushed when
                    // it changes, and refreshed (its error rate) with every
                    // spectrum frame for anyone reading the REST status.
                    // The error rate alone moves every block; it goes out at
                    // most every two seconds, and only when it has moved.
                    let generation = rds.generation();
                    let bler = rds.station().map_or(0.0, |st| st.bler);
                    let bler_due = rds_pushed.0.elapsed() >= Duration::from_secs(2) && (bler - rds_pushed.1).abs() >= 0.02;
                    if generation != *rds_seen || bler_due {
                        let changed = generation != *rds_seen;
                        *rds_seen = generation;
                        *rds_pushed = (Instant::now(), bler);
                        if let Ok(mut s) = status.lock() {
                            let kept = s.rds.as_ref().map_or(0, |o| o.updated_ms);
                            s.rds = rds.station().map(|mut st| {
                                st.updated_ms = if changed { now_ms() } else { kept };
                                st
                            });
                            let _ = events.send(SdrEvent::Status(s.clone()));
                        }
                    }
                }
                ClassificationResult {
                    active: true,
                    protocol: "WFM broadcast".into(),
                    modulation: "Wideband FM".into(),
                    ..Default::default()
                }
            }
        };

        // Packet runs every decoder over the discriminators the classifier
        // built, so whatever is in the passband gets decoded without being
        // told in advance which format it is. APRS reads the 12.5 kHz
        // channel's: Bell 202 at 3 kHz deviation is about ±5 kHz wide, and
        // over the whole 25 kHz channel a packet 7 dB over the noise sat
        // under the discriminator's threshold and never decoded (it does
        // from the narrow one, as a 12.5 kHz radio's TNC copies it). The
        // pagers keep the wide one: 2400 Bd POCSAG and FLEX need the room.
        if let Demod::Packet {
            aprs,
            pocsag,
            flex,
        } = &mut demod
        {
            let disc = classifier.monitor_discriminator_hz();
            let mut aprs_heard = false;
            for packet in aprs.process(classifier.standard_discriminator_hz()) {
                aprs_heard = true;
                decode_history.note(inspect_hz, "APRS");
                let _ = events.send(SdrEvent::Decode {
                    inspect_hz,
                    event: packet_event(&packet),
                });
            }
            // The demod owns the pager banks here; syncs go back to the
            // classifier so its protocol matching still sees them.
            let mut pocsag_sync_baud = None;
            for (baud, dec) in [512u32, 1200, 2400].into_iter().zip(pocsag.iter_mut()) {
                let syncs_before = {
                    let d = dec.diagnostics();
                    d.syncs_512 + d.syncs_1200 + d.syncs_2400
                };
                for msg in dec.process(disc) {
                    decode_history.note(inspect_hz, "POCSAG");
                    let _ = events.send(SdrEvent::Decode {
                        inspect_hz,
                        event: pocsag_event(&msg),
                    });
                }
                let d = dec.diagnostics();
                let syncs_after = d.syncs_512 + d.syncs_1200 + d.syncs_2400;
                if syncs_after > syncs_before {
                    pocsag_sync_baud = Some(d.last_sync_baud.unwrap_or(baud));
                }
            }
            let flex_syncs_before = {
                let d = flex.diagnostics();
                (d.syncs_1600, d.syncs_3200, d.syncs_6400)
            };
            for msg in flex.process(disc) {
                decode_history.note(inspect_hz, "FLEX");
                let _ = events.send(SdrEvent::Decode {
                    inspect_hz,
                    event: flex_event(&msg),
                });
            }
            if let Some(baud) = pocsag_sync_baud {
                classifier.note_pocsag_sync(baud);
            }
            if aprs_heard {
                classifier.note_aprs_frame();
            }
            {
                let d = flex.diagnostics();
                let flex_sync_baud = if d.syncs_6400 > flex_syncs_before.2 {
                    Some(6400)
                } else if d.syncs_3200 > flex_syncs_before.1 {
                    Some(3200)
                } else if d.syncs_1600 > flex_syncs_before.0 {
                    Some(1600)
                } else {
                    None
                };
                if let Some(baud) = flex_sync_baud {
                    classifier.note_flex_sync(baud);
                }
            }
        }

        // Dedicated FLEX mode: decodes only Motorola FLEX traffic on this channel.
        if let Demod::Flex { flex } = &mut demod {
            let disc = classifier.monitor_discriminator_hz();
            let flex_syncs_before = {
                let d = flex.diagnostics();
                (d.syncs_1600, d.syncs_3200, d.syncs_6400)
            };
            for msg in flex.process(disc) {
                decode_history.note(inspect_hz, "FLEX");
                let _ = events.send(SdrEvent::Decode {
                    inspect_hz,
                    event: flex_event(&msg),
                });
            }
            let d = flex.diagnostics();
            let flex_sync_baud = if d.syncs_6400 > flex_syncs_before.2 {
                Some(6400)
            } else if d.syncs_3200 > flex_syncs_before.1 {
                Some(3200)
            } else if d.syncs_1600 > flex_syncs_before.0 {
                Some(1600)
            } else {
                None
            };
            if let Some(baud) = flex_sync_baud {
                classifier.note_flex_sync(baud);
                if (last_flex_inspect_hz - inspect_hz).abs() > 1.0 {
                    flex_carrier_corr_hz = 0.0;
                    last_flex_inspect_hz = inspect_hz;
                }
                if let Some(residual) = d.last_carrier_offset_hz {
                    let residual_f64 = f64::from(residual);
                    if residual_f64.abs() <= 3_000.0 {
                        flex_carrier_corr_hz += residual_f64 * 0.70;
                        flex_carrier_corr_hz = flex_carrier_corr_hz.clamp(-3_000.0, 3_000.0);
                        freq_error_hz = Some(flex_carrier_corr_hz + residual_f64 * 0.30);
                        inspect_chain.set_offset(
                            inspect_hz - tuned_freq
                                - lo_offset_for(current_rate, lo_offset, current_mode)
                                + flex_carrier_corr_hz,
                        );
                    }
                }
            }
        }

        // Execute whatever the scanner asked for during this block. It cannot
        // act mid-arm — the chain is borrowed while its receivers run — so its
        // candidate moves and notices land here, before the scope reads the
        // (one block stale, therefore harmless) chain state.
        for a in std::mem::take(&mut scan_actions) {
            apply_scan_action(
                a,
                &mut InspectParts {
                    inspect_chain: &mut inspect_chain,
                    classifier: &mut classifier,
                    demod: &mut demod,
                    scope_fm: &mut scope_fm,
                    capture: &capture,
                    afc: &mut afc,
                    afc_last_reported: &mut afc_last_reported,
                },
                &mut scan_slots,
                &events,
                &status,
                &mut current_freq,
                tuned_freq,
                &mut inspect_hz,
                &mut pending_tune,
                current_rate,
                lo_offset,
                current_mode,
                &scan_cfg.modes,
            );
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
            SdrMode::Nxdn => {
                // NXDN is C4FM with ±480/±1440 Hz levels: scale against the
                // outer deviation so the four eyes land at ±0.33/±1.0 and
                // carrier drift stays visible without rail-clamping.
                scope_fm.discriminator_hz(&inspect_iq, &mut scope_disc);
                scope_src.extend(scope_disc.iter().map(|&hz| (hz / 1_440.0).clamp(-1.0, 1.0)));
                scope_src_rate = fs_chain;
                scope_dev_scale = 1_440.0;
            }
            SdrMode::Wfm => {
                if let Demod::Wfm { disc, .. } = &demod {
                    // Pre-de-emphasis, so the pilot and subcarriers survive.
                    scope_src
                        .extend(disc.iter().map(|&hz| (hz / WFM_DEVIATION_HZ).clamp(-1.0, 1.0)));
                    scope_src_rate = inspect_chain.fs_out(); // the real rate, not the nominal 256 kHz
                    scope_dev_scale = WFM_DEVIATION_HZ;
                }
            }
            SdrMode::Pager => {
                // Normalised against the channel discriminator's full-scale
                // reference, like every other mode's trace. The raw hertz used
                // to go through the ±1 clamp meant for pre-normalised traces,
                // which squared the waveform off at full scale — a scope that
                // showed noise as a blizzard of rail-to-rail edges.
                scope_src.extend(
                    pager_live_audio
                        .iter()
                        .map(|&hz| hz / scannerd_engine::pager_bank::PAGER_DEVIATION_HZ),
                );
                // The source rate is what the bank really delivers
                // (span/every — 97 523.8 Hz at a 2.048 MS/s span); the
                // decimation below box-averages it down to the mode's
                // 48 kHz scope rate, which is what the axis is labelled with.
                scope_src_rate = pager_bank
                    .as_ref()
                    .map(|b| b.channel_rate())
                    .unwrap_or(scannerd_engine::pager_bank::PAGER_CHANNEL_RATE);
                scope_dev_scale = scannerd_engine::pager_bank::PAGER_DEVIATION_HZ;
            }
            SdrMode::Flex => {
                // For FLEX: provide the monitor discriminator normalized against
                // PAGER_DEVIATION_HZ (8000 Hz). Outer levels (±4.8 kHz) land at ±0.6,
                // inner levels (±1.6 kHz) land at ±0.2, leaving headroom for carrier
                // drift so offsets are visible without rail-clamping.
                let disc = classifier.monitor_discriminator_hz();
                let scale = scannerd_engine::pager_bank::PAGER_DEVIATION_HZ;
                scope_src.extend(disc.iter().map(|&hz| (hz / scale).clamp(-1.0, 1.0)));
                scope_src_rate = fs_chain;
                scope_dev_scale = scale;
            }
            SdrMode::Nfm | SdrMode::Packet | SdrMode::Auto => {
                // The RAW discriminator, not the gated monitor audio: dead air
                // is exactly when the scope matters most, and the noise gate
                // flattens that to zero by design. Discriminator noise IS the
                // activity — its collapse is what shows a carrier arrived.
                //
                // However, when Auto mode has classified an active digital protocol
                // (DMR, P25, NXDN), route the 12.5 kHz channel-filtered standard
                // discriminator scaled by 3000 Hz so the digital eye scope receives
                // the clean, matched baseband rather than wideband adjacent noise.
                let is_digital_active = current_mode == SdrMode::Auto
                    && classification.active
                    && (classification.protocol.contains("DMR")
                        || classification.protocol.contains("P25")
                        || classification.protocol.contains("NXDN"));
                let (disc, scale) = if is_digital_active {
                    (classifier.standard_discriminator_hz(), 3_000.0f64)
                } else {
                    (
                        classifier.monitor_discriminator_hz(),
                        f64::from(classifier.deviation_scale_hz()),
                    )
                };
                scope_src.extend(disc.iter().map(|&hz| (hz as f64 / scale).clamp(-1.0, 1.0) as f32));
                scope_src_rate = fs_chain;
                scope_dev_scale = scale as f32;
            }
            SdrMode::Scan => {
                // Whatever the scan routed this block — digital voice at 8 kHz
                // or an analog path at the chain rate — tagged with the rate
                // it actually arrived at, not what the demodulator says now.
                scope_src.extend_from_slice(&audio_buf);
                scope_src_rate = if scan_audio_rate > 0.0 {
                    scan_audio_rate
                } else {
                    fs_chain
                };
                scope_dev_scale = 0.0;
            }
            _ => {
                scope_src.extend_from_slice(&audio_buf);
                scope_src_rate = demod.audio_rate(fs_chain);
                scope_dev_scale = 0.0;
            }
        }

        // PAGER's monitor audio is the live channel's discriminator, which
        // runs at the bank's channel rate — labelling it with the inspect
        // chain's rate played every page back roughly an octave slow.
        //
        // SCAN's routed audio likewise carries its own rate: the digital
        // receivers deliver 8 kHz voice while the analog paths deliver the
        // chain rate, and the demodulator's answer flips depending on
        // whether it is still in call.
        let audio_rate = if current_mode == SdrMode::Pager {
            pager_bank
                .as_ref()
                .map(|b| b.channel_rate())
                .unwrap_or(fs_chain)
        } else if current_mode == SdrMode::Scan && scan_audio_rate > 0.0 {
            scan_audio_rate
        } else {
            demod.audio_rate(fs_chain)
        };
        if !audio_buf.is_empty() && audio_rate > 0.0 {
            let pcm = |x: &f32| (x.clamp(-1.0, 1.0) * 28_000.0) as i16;
            // WFM carries what the stereo decoder measured with every frame,
            // and the L, R pair when it delivered one for this whole block.
            let stereo = match &demod {
                Demod::Wfm { audio, stereo, .. } if current_mode == SdrMode::Wfm => Some(StereoAudio {
                    samples: if stereo.len() == 2 * audio_buf.len() { stereo.iter().map(pcm).collect() } else { Vec::new() },
                    status: audio.status(),
                }),
                _ => None,
            };
            let _ = audio_tx.send(AudioFrame {
                rx: 0,
                rate_hz: audio_rate.round() as u32,
                samples: audio_buf.iter().map(pcm).collect(),
                stereo,
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
            if current_mode == SdrMode::Flex && event.protocol != "FLEX" {
                continue;
            }
            let _ = events.send(SdrEvent::Decode { inspect_hz, event });
        }

        if frame_due && !pwr.is_empty() {
            // Carry the remainder so the long-run rate holds, but never
            // owe more than one frame: after a stall (reopen, rate change)
            // the display resumes rather than bursting to catch up. A
            // remainder that is a sliver of a delivery is dropped: carried,
            // it added up to a frame one delivery short every few seconds,
            // and an even cadence is worth more than the last 0.3% of rate.
            let rem = frame_pending_samples.saturating_sub(frame_quantum);
            frame_pending_samples = if frame_pending_samples >= 2 * frame_quantum || rem < block_samples / 8 { 0 } else { rem };
            if let Ok(mut s) = status.lock() {
                s.inspect_rate_hz = inspect_chain.fs_out();
                s.fft_size = zoom.as_ref().map_or(fft_size, |z| z.plan.fft_size);
                s.dropped_blocks = dropped_blocks;
                s.lagged_blocks = lagged_blocks;
                s.lost_samples = lost_samples;
                s.gap_events = gap_events;
                s.freq_error_hz = freq_error_hz;
                s.flex_diag = if let Demod::Flex { flex } = &demod {
                    Some(flex.diagnostics().clone())
                } else {
                    None
                };
                match &demod {
                    Demod::Wfm { rds, audio, eq, .. } => {
                        s.rds = rds.station().map(|mut st| {
                            st.updated_ms = s.rds.as_ref().map_or(0, |o| o.updated_ms);
                            st
                        });
                        s.stereo = Some(with_equalizer(audio.status(), eq.as_deref()));
                    }
                    // Out of WFM: the station's RDS and stereo go, and
                    // clients hear so.
                    _ if s.rds.is_some() || s.stereo.is_some() => {
                        s.rds = None;
                        s.stereo = None;
                        let _ = events.send(SdrEvent::Status(s.clone()));
                    }
                    _ => {}
                }
                // Out of the text and picture modes, their reading goes.
                if s.digital.is_some() && !matches!(&demod, Demod::Ssb { hf: Some(_), .. } | Demod::Am { hf: Some(_), .. }) {
                    s.digital = None;
                    let _ = events.send(SdrEvent::Status(s.clone()));
                }
                // Running every decoder at once costs about a core, and scan
                // runs a rig per configured slot — the heaviest mode there
                // is. If the span is wide enough that blocks are being
                // dropped, the operator should hear it from the receiver
                // rather than wonder why decodes are patchy.
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
                } else if current_mode == SdrMode::Scan && lagging {
                    s.error = Some(format!(
                        "SCAN cannot keep up at {:.3} MS/s — dropping blocks; \
                         try fewer slots or a narrower span",
                        current_rate / 1e6
                    ));
                } else if s.error.as_deref().is_some_and(|e| {
                    e.starts_with("AUTO cannot keep up") || e.starts_with("SCAN cannot keep up")
                }) {
                    s.error = None;
                }
            }
            smooth_bins(&pwr, 3, &mut smoothed);

            let lo_off = window.lo_off;
            // Which spectrum the window is cut from. Zoomed in, the zoom
            // path's, once it has a frame of its own: its transform spans
            // the decimated rate with the window's center at DC. Otherwise
            // the full span's, with the hardware center at DC (see
            // ShownWindow).
            let (src_smoothed, src_pwr, src_peak, src_rate, src_off, src_dc): (&[f32], &[f32], &[f32], f64, f64, f64) =
                match zoom.as_ref() {
                    Some(z) if z.ready => (&z.smoothed, &z.pwr, &z.pwr_peak, z.plan.rate_hz, 0.0, window.shown_centre),
                    _ => (&smoothed, &pwr, &pwr_peak, current_rate, window.mix_offset_hz(), tuned_freq + lo_off),
                };
            let n = src_smoothed.len();
            let (lo_bin, hi_bin) = window_bins(src_rate, n, src_off, window.shown_rate);
            // From here on the window is the bins in the frame, not the
            // request: its axis is theirs, so what the viewers draw and
            // what is measured below both sit exactly on the signal.
            let axis = frame_axis(src_dc, src_rate, n, lo_bin, hi_bin);
            let (shown_centre, shown_rate) = (axis.center_hz, axis.rate_hz);
            let mut shown_buf = src_smoothed[lo_bin..hi_bin].to_vec();
            // Guard scales with the bin width so the notch stays a few bins
            // wide whatever the span and FFT size are.
            let guard_hz = (shown_rate / shown_buf.len().max(1) as f64 * 3.0).max(4_000.0);
            notch_display(&mut shown_buf, shown_rate, &spurs, guard_hz);
            // With the LO parked off the channel, its DC residue is a
            // receiver artifact sitting in plain view a fixed distance from
            // the receive frequency. Notch it for display like a spur. Never
            // when the LO is not offset: DC is then the tuned frequency, and
            // notching it would hide the very signal being received.
            let dc_rel = tuned_freq + lo_off - shown_centre;
            // The residue is a hump about ±15 kHz wide (see zero_if_offset).
            let dc_guard = (shown_rate / shown_buf.len().max(1) as f64 * 1.5).max(DC_NOTCH_HZ);
            if lo_off != 0.0 {
                notch_display(&mut shown_buf, shown_rate, &[dc_rel], dc_guard);
            }
            let shown: &[f32] = &shown_buf;
            let wf_peak_shown: Vec<f32> = if src_peak.len() == n {
                let mut v = src_peak[lo_bin..hi_bin].to_vec();
                notch_display(&mut v, shown_rate, &spurs, guard_hz);
                if lo_off != 0.0 {
                    notch_display(&mut v, shown_rate, &[dc_rel], dc_guard);
                }
                v
            } else {
                Vec::new()
            };
            // The waterfall's row: the same window, notched the same way,
            // but neither bin-smoothed nor averaged across frames.
            let wf_row: Vec<f32> = {
                let mut v = src_pwr[lo_bin..hi_bin].to_vec();
                notch_display(&mut v, shown_rate, &spurs, guard_hz);
                if lo_off != 0.0 {
                    notch_display(&mut v, shown_rate, &[dc_rel], dc_guard);
                }
                v
            };
            // Update the per-bin floor from this frame before anything reads it.
            let floor = bin_floor.update(shown);
            // The shared sorted copy backs every per-frame measurement below.
            // It must reflect THIS frame: a stale copy made the noise floor —
            // and with it every SNR, peak threshold and AFC decision — a
            // snapshot from whenever the window last changed. The floor
            // itself is smoothed over time by `bin_floor` instead.
            sorted_scratch.clear();
            sorted_scratch.extend_from_slice(shown);
            sorted_scratch.sort_unstable_by(f32::total_cmp);

            // Where the hardware's own artefacts land in display coordinates:
            // its DC/LO leakage, and the ±fs/4 images either side of it.
            let spurs = [
                lo_off,
                lo_off + current_rate / 4.0,
                lo_off - current_rate / 4.0,
            ];
            // Measured over the passband the mode listens to: for sideband
            // that is beside the carrier, not centred on it.
            let (channel_dbfs, noise_dbfs) = channel_level(
                shown,
                shown_rate,
                inspect_hz - shown_centre + f64::from(current_mode.passband_offset_hz()),
                f64::from(current_mode.bandwidth_hz()),
                &mut sorted_scratch,
            );
            last_snr_db = channel_dbfs - noise_dbfs;
            // `shown` is re-centred on the display centre (which sits away
            // from the dial whenever the inspect channel is off-tune), and
            // the spur offsets above are display-relative — so the peak
            // search must be given the same centre, or every marker lands
            // at `centre_off` the wrong frequency and the spur mask misses.
            let peaks = find_peaks(
                shown,
                shown_centre,
                shown_rate,
                &spurs,
                floor,
                &decode_history,
            );
            decode_history.prune();

            // Track carrier offset for frequency calibration (PPM correction) and dial accuracy.
            if (last_carrier_inspect_hz - inspect_hz).abs() > 10.0 {
                freq_error_hz = None;
                last_carrier_err_at = None;
                last_carrier_inspect_hz = inspect_hz;
            }

            // 1. Check protocol-specific carrier tracking decoders if active
            let decoder_err = match current_mode {
                SdrMode::Pager => pager_bank.as_ref().and_then(|b| b.carrier_offset_hz()),
                SdrMode::P25 => {
                    if let Demod::P25 { rx } = &demod {
                        rx.carrier_offset_hz()
                    } else {
                        None
                    }
                }
                SdrMode::Dmr => {
                    if let Demod::Dmr { rx } = &demod {
                        rx.carrier_offset_hz()
                    } else {
                        None
                    }
                }
                SdrMode::Flex => {
                    if let Demod::Flex { flex } = &demod {
                        flex.diagnostics().last_carrier_offset_hz.map(f64::from)
                    } else {
                        None
                    }
                }
                _ => None,
            };

            // Sideband has no carrier: the voice energy sits wholly to one
            // side of the dial, and a centroid of it (or of whatever peak
            // is nearest) is a number that looks like a crystal error and
            // is not. One such reading, calibrated from, left a 25 ppm
            // correction on an RSP1A whose TCXO is good to 0.5. So nothing
            // is measured in USB/LSB; the dial is checked in AM or FM.
            let sideband = current_mode.is_sideband();

            // 2. Direct in-channel spectral centroid if channel energy stands over the noise floor
            let in_channel_offset = if !sideband && channel_dbfs - noise_dbfs > 3.0 {
                carrier_offset_hz(
                    shown,
                    shown_rate,
                    inspect_hz - shown_centre,
                    f64::from(current_mode.bandwidth_hz()),
                    &mut sorted_scratch,
                )
            } else {
                None
            };

            // 3. Nearby carrier peak search: detects carriers when crystal error has shifted
            // the signal outside the narrow channel filter (up to ±60 kHz of inspect_hz).
            let nearby_peak_offset = if !sideband && in_channel_offset.is_none() && decoder_err.is_none() {
                let max_search_hz = (inspect_hz * 120e-6).clamp(30_000.0, 60_000.0);
                peaks
                    .iter()
                    .filter(|p| {
                        p.snr_db >= 4.0 && (p.freq_hz - inspect_hz).abs() <= max_search_hz
                    })
                    .min_by(|a, b| {
                        (a.freq_hz - inspect_hz)
                            .abs()
                            .total_cmp(&(b.freq_hz - inspect_hz).abs())
                    })
                    .and_then(|p| {
                        let peak_offset_display = p.freq_hz - shown_centre;
                        carrier_offset_hz(
                            shown,
                            shown_rate,
                            peak_offset_display,
                            25_000.0,
                            &mut sorted_scratch,
                        )
                        .map(|fine_corr| (p.freq_hz + fine_corr) - inspect_hz)
                    })
            } else {
                None
            };

            let raw_err = decoder_err
                .or(in_channel_offset)
                .or(nearby_peak_offset);

            if let Some(err) = raw_err {
                freq_error_hz = Some(match freq_error_hz {
                    Some(prev) => prev + (err - prev) * 0.20,
                    None => err,
                });
                last_carrier_err_at = Some(Instant::now());
            } else if last_carrier_err_at.is_some_and(|t| t.elapsed() > Duration::from_secs(5)) {
                freq_error_hz = None;
            }

            // Voice scan: look for candidates in the fresh FULL-span spectrum
            // (the display peaks above were found in the possibly-zoomed
            // window; the sweep works the whole window the tuner can cover),
            // then act on whatever the machine asked for and publish its
            // status when it changed.
            if current_mode == SdrMode::Scan {
                // Refresh every running slot's channel SNR against the fresh
                // spectrum — the figure the squelch and the receivers see
                // until the next frame, and the same measurement the
                // candidate finder judged the carrier by.
                for slot in scan_slots.iter_mut().flatten() {
                    let (ch, nz) = channel_level(
                        &smoothed,
                        current_rate,
                        slot.freq_hz - tuned_freq,
                        f64::from(current_mode.bandwidth_hz()),
                        &mut sorted_scratch,
                    );
                    slot.snr_db = ch - nz;
                }
                if let Some(sc) = scanner.as_mut() {
                    let acts = sc.observe_frame(
                        scan_t0.elapsed().as_secs_f64(),
                        tuned_freq,
                        current_rate,
                        display_rate_for(current_rate, lo_offset, current_mode) * 0.49,
                        &smoothed,
                        &spurs,
                    );
                    for a in acts {
                        apply_scan_action(
                            a,
                            &mut InspectParts {
                                inspect_chain: &mut inspect_chain,
                                classifier: &mut classifier,
                                demod: &mut demod,
                                scope_fm: &mut scope_fm,
                                capture: &capture,
                                afc: &mut afc,
                                afc_last_reported: &mut afc_last_reported,
                            },
                            &mut scan_slots,
                            &events,
                            &status,
                            &mut current_freq,
                            tuned_freq,
                            &mut inspect_hz,
                            &mut pending_tune,
                            current_rate,
                            lo_offset,
                            current_mode,
                            &scan_cfg.modes,
                        );
                    }
                    if sc.take_dirty() {
                        let mut s = status.lock().unwrap();
                        s.scan = Some(sc.status(scan_t0.elapsed().as_secs_f64()));
                        let _ = events.send(SdrEvent::Status(s.clone()));
                    }
                }
            }
            // Frame-to-frame integration for the display: exponential per-bin
            // averaging pulls steady weak carriers out of the flicker, and
            // the max-hold trace keeps a fleeting signal visible. Driven by
            // the AVERAGE softkey (off / 1 s / 4 s); max-hold always runs and
            // decays slowly so it never lies about the present.
            {
                let k = match avg_mode {
                    AvgMode::Off => None,
                    AvgMode::Slow => Some(0.08),   // ~0.5 s at 25 fps
                    AvgMode::Deeper => Some(0.02), // ~2 s
                };
                if averaged.len() != shown.len() {
                    averaged.clear();
                    averaged.extend_from_slice(shown);
                } else if let Some(k) = k {
                    for (a, &s) in averaged.iter_mut().zip(shown) {
                        *a += (s - *a) * k;
                    }
                } else {
                    averaged.copy_from_slice(shown);
                }
                if max_hold.len() != shown.len() {
                    max_hold.clear();
                    max_hold.extend_from_slice(shown);
                } else {
                    for (m, &s) in max_hold.iter_mut().zip(shown) {
                        let decayed = *m - 0.15;
                        *m = if s > decayed { s } else { decayed };
                    }
                }
            }
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

            let ev = SdrEvent::Fft {
                center_hz: shown_centre,
                rate_hz: shown_rate,
                pwr: if matches!(avg_mode, AvgMode::Off) {
                    shown.to_vec()
                } else {
                    averaged.clone()
                },
                max_hold: max_hold.clone(),
                wf_peak: wf_peak_shown,
                wf_row,
                peak_iq,
                inspect_hz,
                classification: Some(classification),
                peaks,
                scope: scope.iter().copied().collect(),
                scope_rate_hz: current_mode.scope_rate_hz().min(scope_src_rate as f32),
                scope_kind: current_mode.scope_kind(),
                symbol_rate_hz: if current_mode == SdrMode::Flex {
                    if let Demod::Flex { flex } = &demod {
                        flex.diagnostics().last_symbol_rate.map(|r| r as f32).unwrap_or(1_600.0)
                    } else {
                        current_mode.symbol_rate_hz()
                    }
                } else {
                    current_mode.symbol_rate_hz()
                },
                channel_dbfs,
                noise_dbfs,
            };
            // Encode once here and broadcast the bytes: every client sends
            // the identical frame, and the broadcast clone of this one Vec is
            // cheaper than cloning the whole event and re-encoding it per
            // client. A frame that will not encode falls back to the JSON
            // path rather than going dark.
            let _ = events.send(match encode_fft_frame(&ev) {
                Some(frame) => SdrEvent::FftBytes { frame: frame.into() },
                None => ev,
            });

            peak_iq = 0.0;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {

    #[test]
    fn audio_frames_read_back_through_the_client_decoder() {
        use usdr_proto::{Binary, decode_binary};
        let read = |b: Vec<u8>| match decode_binary(&b) {
            Ok(Binary::Audio(a)) => a,
            other => panic!("{other:?}"),
        };
        let lr = vec![i16::MIN, i16::MAX, -7, 7, 28_000, -28_000];
        let status = StereoStatus { pilot: true, blend: 1.0, snr_db: Some(52.4), pilot_dev_hz: Some(7_502.0), ..Default::default() };
        let f = AudioFrame {
            rx: 0,
            rate_hz: 51_200,
            samples: vec![-32_768, 32_767, 0],
            stereo: Some(StereoAudio { samples: lr.clone(), status }),
        };
        // A client that asked for stereo: both channels, bit for bit.
        let a = read(encode_audio_frame(&f, true));
        assert_eq!((a.rx, a.rate_hz, a.channels(), &a.samples), (0, 51_200, 2, &lr));
        let st = a.stereo.unwrap();
        assert!(st.pilot && st.blend == 1.0);
        assert_eq!((st.snr_db, st.pilot_dev_hz), (Some(52.0), Some(7_502.0)));
        // One that did not, and the page: the mono frame they always had.
        let a = read(encode_audio_frame(&f, false));
        assert_eq!((a.channels(), a.samples.as_slice(), a.stereo), (1, &[-32_768i16, 32_767, 0][..], None));
        assert_eq!(encode_audio_frame(&f, false)[0], 0x01);
        // WFM without a pilot, to a stereo client: one channel, the reading
        // says so, and nothing unmeasured is claimed.
        let mono = AudioFrame {
            stereo: Some(StereoAudio { samples: Vec::new(), status: StereoStatus::default() }),
            ..f.clone()
        };
        let a = read(encode_audio_frame(&mono, true));
        let st = a.stereo.unwrap();
        assert_eq!((a.channels(), a.samples.len()), (1, 3));
        assert!(!st.pilot && st.snr_db.is_none() && st.pilot_dev_hz.is_none());
        // Receiver B keeps its tagged mono frame.
        let b = AudioFrame { rx: 1, rate_hz: 16_000, samples: vec![1, 2], stereo: None };
        let a = read(encode_audio_frame(&b, true));
        assert_eq!((a.rx, a.samples.as_slice(), a.stereo), (1, &[1i16, 2][..], None));
    }

    #[test]
    fn filters_follow_the_mode_rules_and_never_move_the_lo() {
        // Sideband takes both edges, on its own side of the carrier.
        assert_eq!(SdrMode::Usb.normalize_filter(500.0, 2_000.0), Ok((500.0, 2_000.0)));
        assert!(SdrMode::Usb.normalize_filter(-500.0, 2_000.0).is_err());
        assert_eq!(SdrMode::Lsb.normalize_filter(-2_400.0, -300.0), Ok((-2_400.0, -300.0)));
        assert!(SdrMode::Lsb.normalize_filter(-100.0, 0.0).is_err(), "narrower than 200 Hz");
        // AM is centered whatever is asked, and clamped to its range.
        assert_eq!(SdrMode::Am.normalize_filter(0.0, 6_000.0), Ok((-3_000.0, 3_000.0)));
        assert_eq!(SdrMode::Am.normalize_filter(-50_000.0, 50_000.0), Ok((-10_000.0, 10_000.0)));
        // Digital decoders keep their filters.
        assert!(SdrMode::P25.normalize_filter(-3_000.0, 3_000.0).is_err());

        let before = lo_offset_for(5e6, true, SdrMode::Wfm);
        SdrMode::Wfm.set_filter(Some((-60_000.0, 60_000.0)));
        assert_eq!(SdrMode::Wfm.bandwidth_hz(), 120_000.0);
        assert_eq!(lo_offset_for(5e6, true, SdrMode::Wfm), before, "the LO plan is laid out on the mode's own width");
        SdrMode::Wfm.set_filter(None);
        assert_eq!(SdrMode::Wfm.bandwidth_hz(), WFM_BANDWIDTH_HZ);

        SdrMode::Lsb.set_filter(Some((-2_000.0, -1_000.0)));
        assert_eq!(SdrMode::Lsb.passband_offset_hz(), -1_500.0);
        assert_eq!(SdrMode::Lsb.chain_bandwidth_hz(), 4_800.0);
        SdrMode::Lsb.set_filter(None);
    }

    #[test]
    fn sideband_and_ft8_filters_reach_five_khz_and_take_a_shape() {
        // Extended SSB and the hi-fi preset fit; past 5 kHz is clamped.
        assert_eq!(SdrMode::Usb.normalize_filter(50.0, 4_500.0), Ok((50.0, 4_500.0)));
        assert_eq!(SdrMode::Lsb.normalize_filter(-4_000.0, -50.0), Ok((-4_000.0, -50.0)));
        assert_eq!(SdrMode::Usb.normalize_filter(50.0, 9_000.0), Ok((50.0, 5_000.0)));
        // FT8 opens to the whole sub-band, above the dial only.
        assert_eq!(SdrMode::Ft8.normalize_filter(100.0, 4_500.0), Ok((100.0, 4_500.0)));
        assert!(SdrMode::Ft4.normalize_filter(-200.0, 3_000.0).is_err());
        assert!(SdrMode::Ft8.normalize_filter(1_000.0, 1_300.0).is_err(), "narrower than 500 Hz");
        // The other data modes keep the filter their decoder was built for.
        assert!(SdrMode::Rtty.normalize_filter(100.0, 4_000.0).is_err());
        // The chain carries the widest filter with room for its skirt.
        SdrMode::Usb.set_filter(Some((50.0, 5_000.0)));
        assert!(SdrMode::Usb.chain_bandwidth_hz() >= 2.0 * 5_000.0 + 2.0 * FilterShape::Soft.skirt_hz() as f32);
        SdrMode::Usb.set_filter(None);

        // Shapes: per mode, normal unless set, and only where the mode
        // takes one.
        assert_eq!(SdrMode::Lsb.filter_shape(), FilterShape::Normal);
        SdrMode::Lsb.set_filter_shape(Some(FilterShape::Soft));
        SdrMode::Rtty.set_filter_shape(Some(FilterShape::Sharp));
        assert_eq!(SdrMode::Lsb.filter_shape(), FilterShape::Soft);
        assert_eq!(SdrMode::Usb.filter_shape(), FilterShape::Normal);
        assert_eq!(SdrMode::Rtty.filter_shape(), FilterShape::Normal);
        let mut st = SdrStatus::default();
        stamp_filter(&mut st, SdrMode::Lsb);
        assert_eq!(st.filter_shape, FilterShape::Soft);
        assert_eq!(serde_json::to_value(&st).unwrap()["filter_shape"], "soft");
        SdrMode::Lsb.set_filter_shape(None);
        SdrMode::Rtty.set_filter_shape(None);
        assert_eq!(SdrMode::Lsb.filter_shape(), FilterShape::Normal);
    }
    use super::*;

    /// The zoom path's bin shrinks with the zoom: between a half and one
    /// display pixel wide at every zoom, where one FFT over the span, capped
    /// at 8192, left a 977 Hz bin at 8 MS/s whatever the zoom.
    #[test]
    fn zoom_bins_narrow_with_the_zoom() {
        let bins = 2048usize;
        for &fs in &[8.0e6, 2.4e6, 2.048e6] {
            assert_eq!(fft_size_for(bins), bins, "the 1x view keeps one bin per pixel");
            for &zoom in &[2.0, 3.0, 4.0, 8.0, 11.3, 16.0, 32.0] {
                let shown = fs / zoom;
                let p = zoom_plan(fs, shown, bins);
                let pixel_hz = shown / bins as f64;
                assert!(
                    p.bin_hz() <= pixel_hz * 1.0001 && p.bin_hz() > pixel_hz / 2.0,
                    "fs {fs} zoom {zoom}: bin {} Hz for a {pixel_hz} Hz pixel ({p:?})",
                    p.bin_hz()
                );
                assert!(p.rate_hz >= shown * ZOOM_GUARD, "guard band kept: {p:?}");
                assert!(p.stages == 0 || p.rate_hz < shown * ZOOM_GUARD * 2.0, "no stage left on the table: {p:?}");
                assert!(p.fft_size <= if p.stages == 0 { 8192 } else { 16384 }, "{p:?}");
                assert!((p.fs_in / 2f64.powi(p.stages as i32) - p.rate_hz).abs() < 1e-6);
            }
        }
        // The case that prompted this: 40 m at 8 MS/s, zoomed 32x on a
        // 2048-bin display. 250 kHz across 2048 bins of 122 Hz, where the
        // full-span FFT gave 256 bins of 977 Hz.
        let p = zoom_plan(8.0e6, 8.0e6 / 32.0, 2048);
        assert_eq!((p.stages, p.rate_hz, p.fft_size), (4, 500_000.0, 4096));
        assert!((p.bin_hz() - 122.07).abs() < 0.01, "{}", p.bin_hz());
        let old_bin = 8.0e6 / 8192.0;
        assert!(old_bin / p.bin_hz() > 7.9);
    }

    /// Run a span through a zoom path the way the loop does, in blocks, and
    /// take one frame's spectrum.
    fn zoom_take(fs: f64, zoom: f64, bins: usize, mix_offset_hz: f64, tones: &[(f64, f32)], seconds: f64) -> ZoomPath {
        let shown = fs / zoom;
        let mut z = ZoomPath::new(zoom_plan(fs, shown, bins), shown, mix_offset_hz);
        let n = (fs * seconds) as usize;
        let mut block = Vec::with_capacity(16_384);
        let mut t = 0usize;
        while t < n {
            block.clear();
            for i in t..(t + 16_384).min(n) {
                let mut s = num_complex::Complex32::new(0.0, 0.0);
                for &(hz, amp) in tones {
                    let ph = 2.0 * std::f64::consts::PI * hz * i as f64 / fs;
                    s += num_complex::Complex32::new((ph.cos() * f64::from(amp)) as f32, (ph.sin() * f64::from(amp)) as f32);
                }
                block.push(s);
            }
            z.ingest(&block);
            t += block.len();
        }
        assert!(z.take() > 0, "a frame's worth of samples completes at least one segment");
        assert!(z.ready);
        z
    }

    /// A tone in the window lands in the bin its frequency says, at its
    /// level, after the mix and the halfband cascade; one outside the
    /// window, where it would fold onto the window without the filters,
    /// is stopped.
    #[test]
    fn a_tone_lands_in_its_zoom_bin_after_mix_and_decimation() {
        let (fs, zoom, bins) = (8.0e6, 32.0, 2048usize);
        let plan = zoom_plan(fs, fs / zoom, bins);
        assert_eq!(plan.fft_size, 4096);
        let bin_hz = plan.bin_hz();
        // The window sits 1 MHz above the hardware center, as it does with
        // the receive frequency off the LO; the tone 303 bins above the
        // window's center, on a bin so the level reads true.
        let mix = 1.0e6;
        let tone_off = 303.0 * bin_hz;
        let z = zoom_take(fs, zoom, bins, mix, &[(mix + tone_off, 0.5)], 0.03);
        let n = z.pwr.len();
        assert_eq!(n, plan.fft_size);
        let (peak_bin, &peak_db) = z.pwr.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap();
        let expect = n / 2 + 303;
        assert!(
            peak_bin.abs_diff(expect) <= 1,
            "tone at +{tone_off:.0} Hz read in bin {peak_bin}, expected {expect}"
        );
        // 0.5 amplitude is -6 dBFS; the cascade has unity gain in its passband.
        assert!((peak_db + 6.0).abs() < 1.0, "tone level {peak_db:.2} dBFS");
        // Away from the tone the window is clean to the window's sidelobes.
        let far = z.pwr[..expect - 40].iter().chain(&z.pwr[expect + 40..]).cloned().fold(f32::MIN, f32::max);
        assert!(far < -80.0, "leakage {far:.1} dBFS");
        // The same read through the frame's own mapping: the window is the
        // middle 250 kHz of the 500 kHz the transform spans, so the tone
        // sits at its offset from the window's center.
        let shown = fs / zoom;
        let lo_bin = ((-shown / 2.0 / plan.rate_hz + 0.5) * n as f64).round() as usize;
        let hi_bin = ((shown / 2.0 / plan.rate_hz + 0.5) * n as f64).round() as usize;
        assert_eq!(hi_bin - lo_bin, bins, "the window covers one bin per display pixel");
        let row = &z.pwr[lo_bin..hi_bin];
        let hz_of = |i: usize| -shown / 2.0 + (i as f64 + 0.5) * shown / row.len() as f64;
        let (i, _) = row.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap();
        assert!((hz_of(i) - tone_off).abs() <= bin_hz, "row peak at {:.0} Hz, tone at {tone_off:.0}", hz_of(i));

        // Aliases. A tone the decimation would fold onto the window — at
        // the decimated rate less its offset, on either side — must not
        // show there. At the edge of the guard band the last stage is just
        // reaching its stopband; deeper in, the floor is the window's.
        for (alias_off, limit_db) in [(plan.rate_hz - 125_000.0, -75.0), (plan.rate_hz - 60_000.0, -80.0), (-(plan.rate_hz - 20_000.0), -80.0)] {
            let z = zoom_take(fs, zoom, bins, mix, &[(mix + alias_off, 0.5)], 0.03);
            let folded = if alias_off > 0.0 { alias_off - plan.rate_hz } else { alias_off + plan.rate_hz };
            let bin = ((folded / plan.rate_hz + 0.5) * n as f64).round() as usize;
            let worst = z.pwr[bin.saturating_sub(3)..(bin + 4).min(n)].iter().cloned().fold(f32::MIN, f32::max);
            assert!(worst < limit_db, "tone at {alias_off:.0} Hz folds to {folded:.0} Hz at {worst:.1} dBFS (limit {limit_db})");
        }
    }

    /// What the zoom path costs per second of span at the heaviest zoom,
    /// against the full-span transform it replaces there. Run with
    /// `cargo test --release -p usdr-server zoom_path_cost -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn zoom_path_cost() {
        use std::time::Instant;
        let (fs, bins) = (8.0e6, 2048usize);
        let block: Vec<num_complex::Complex32> = (0..16_384)
            .map(|i| num_complex::Complex32::new((i as f32 * 0.37).sin() * 0.3, (i as f32 * 0.11).cos() * 0.3))
            .collect();
        let blocks = (fs / block.len() as f64) as usize;
        for zoom in [2.0, 8.0, 32.0] {
            let shown = fs / zoom;
            let plan = zoom_plan(fs, shown, bins);
            let mut z = ZoomPath::new(plan, shown, 1.0e6);
            let at = Instant::now();
            for _ in 0..blocks {
                z.ingest(&block);
            }
            z.take();
            let ms = at.elapsed().as_secs_f64() * 1000.0;
            println!("zoom {zoom:>4}x: {plan:?} taps {:?}: {ms:.0} ms per second of span", z.decim.taps());
            // And its parts.
            let mut nco = scannerd_dsp::Nco::new();
            nco.set_freq(1.0e6, fs);
            let mut mixed = Vec::new();
            let at = Instant::now();
            for _ in 0..blocks {
                nco.mix(&block, &mut mixed);
            }
            let mix_ms = at.elapsed().as_secs_f64() * 1000.0;
            let mut decim = scannerd_dsp::HalfbandChain::new(fs, shown / 2.0, plan.stages);
            let mut out = Vec::new();
            let at = Instant::now();
            for _ in 0..blocks {
                decim.process(&block, &mut out);
            }
            let decim_ms = at.elapsed().as_secs_f64() * 1000.0;
            println!("             mix {mix_ms:.0} ms, decimate {decim_ms:.0} ms");
        }
        // The old way: one 8192-point transform over the span at full duty.
        let mut full = Spectrum::new(8192);
        let at = Instant::now();
        for _ in 0..blocks {
            full.ingest_welch(&block);
        }
        let mut out = Vec::new();
        full.take_dbfs(&mut out, None);
        println!("full-span 8192: {:.0} ms per second of span", at.elapsed().as_secs_f64() * 1000.0);
    }

    /// The frame's own axis puts a tone exactly where it is, at every zoom
    /// and wherever the window sits: the stamped center and rate describe
    /// the bins in the frame, not the window that was asked for. Stamping
    /// the request drew signals up to a bin off (half a bin from rounding
    /// the first bin, up to another half from the two edges rounding
    /// differently) — 488 Hz against a 3.6 kHz passband at 32x on 40 m.
    #[test]
    fn the_frame_axis_puts_a_tone_where_its_bin_says() {
        let (fs, bins) = (8.0e6, 2048usize);
        let peak_of = |row: &[f32]| row.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
        // Zoomed: the window's center is at the zoom path's DC.
        for (zoom, mix, k) in [(32.0, 1.0e6, 300i64), (32.0, -2.3e6, -517), (8.0, 0.0, 777), (2.0, 123_456.7, -1_013), (11.3, 0.9e6, 41)] {
            let shown = fs / zoom;
            let plan = zoom_plan(fs, shown, bins);
            let bin = plan.bin_hz();
            // The tone on a bin of the zoom transform, so the read is exact.
            let tone = mix + k as f64 * bin;
            let z = zoom_take(fs, zoom, bins, mix, &[(tone, 0.5)], 0.03);
            let n = z.pwr.len();
            let (lo, hi) = window_bins(plan.rate_hz, n, 0.0, shown);
            let axis = frame_axis(mix, plan.rate_hz, n, lo, hi);
            let row = &z.pwr[lo..hi];
            assert!(row.len() >= bins && row.len() < 2 * bins, "{} bins for {bins} pixels", row.len());
            let i = peak_of(row);
            let at = axis.bin_hz(i, row.len());
            assert!((at - tone).abs() < bin * 1e-3, "zoom {zoom} window {mix}: tone {tone:.1} drawn at {at:.1} ({axis:?}, bin {i} of {})", row.len());
            // The axis spans the bins, not the request: a bin width less
            // than the window asked for, and never more.
            assert!((axis.rate_hz - (row.len() - 1) as f64 * bin).abs() < 1e-6);
            assert!(axis.rate_hz <= shown);
        }
        // 1x: the full span with the LO 100 kHz off the receive frequency,
        // as the zero-IF plan parks it. The frame's DC is the hardware
        // center; the window is the part either side of the dial.
        let (tuned, lo_off) = (7.150e6, 100_000.0);
        let n = fft_size_for(bins);
        let full = Spectrum::new(n);
        let mut spectrum = full;
        let bin = fs / n as f64;
        // On a bin of the full-span transform, whose DC is the hardware center.
        let tone = tuned + lo_off + 234.0 * bin;
        let mut block = Vec::new();
        for i in 0..(fs * 0.02) as usize {
            let ph = 2.0 * std::f64::consts::PI * (tone - (tuned + lo_off)) * i as f64 / fs;
            block.push(num_complex::Complex32::new(ph.cos() as f32 * 0.5, ph.sin() as f32 * 0.5));
        }
        spectrum.ingest_welch(&block);
        let mut pwr = Vec::new();
        assert!(spectrum.take_dbfs(&mut pwr, None) > 0);
        let shown = fs - 2.0 * lo_off;
        let (lo, hi) = window_bins(fs, n, -lo_off, shown);
        let axis = frame_axis(tuned + lo_off, fs, n, lo, hi);
        let row = &pwr[lo..hi];
        let i = peak_of(row);
        let at = axis.bin_hz(i, row.len());
        assert!((at - tone).abs() < bin * 1e-3, "1x: tone {tone:.1} drawn at {at:.1} ({axis:?})");
        // And the old stamp — the request — really was off here.
        let requested = FrameAxis { center_hz: tuned, rate_hz: shown };
        assert!((requested.bin_hz(i, row.len()) - tone).abs() > bin * 0.1);
    }

    /// Moving the window re-tunes the mixer and drops the old window's
    /// accumulators; a plan that has not changed keeps its filters.
    #[test]
    fn a_zoom_retune_moves_the_window_without_a_rebuild() {
        let (fs, zoom, bins) = (8.0e6, 32.0, 2048usize);
        let plan = zoom_plan(fs, fs / zoom, bins);
        let bin_hz = plan.bin_hz();
        let mut z = zoom_take(fs, zoom, bins, 1.0e6, &[(1.0e6 + 100.0 * bin_hz, 0.5)], 0.03);
        let n = z.pwr.len();
        let peak = |p: &[f32]| p.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
        assert_eq!(peak(&z.pwr), n / 2 + 100);
        // Same window: nothing happens.
        z.retune(1.0e6);
        assert!(z.ready);
        // Window 50 bins up: the tone reads 50 bins lower.
        z.retune(1.0e6 + 50.0 * bin_hz);
        assert!(!z.ready);
        assert_eq!(z.plan, plan);
        let moved = zoom_take(fs, zoom, bins, 1.0e6 + 50.0 * bin_hz, &[(1.0e6 + 100.0 * bin_hz, 0.5)], 0.03);
        assert_eq!(peak(&moved.pwr), n / 2 + 50);
    }

    /// Span lock, zoomed in: a retune inside the window leaves the window
    /// where it is, one outside it moves the window onto the receiver, and
    /// a pan stays put while the receiver does not move. Unheld, the window
    /// is always on the receiver.
    #[test]
    fn a_held_zoom_window_stays_until_the_receiver_leaves_it() {
        let (fs, tuned, zoom, mode) = (8.0e6, 100.0e6, 16.0, SdrMode::Nfm);
        let mut v = ZoomView::new(tuned);
        let at = |v: &mut ZoomView, rx: f64| v.window(fs, false, mode, zoom, tuned, rx).shown_centre;
        // Following: the window is on the receiver.
        assert_eq!(at(&mut v, 100.1e6), 100.1e6);
        v.hold = true;
        v.center_hz = 100.1e6;
        let shown = ShownWindow::new(fs, false, mode, zoom, tuned, tuned).shown_rate;
        // Retunes across the window: it holds still.
        for rx in [100.12e6, 100.08e6, 100.1e6 + shown / 2.0 - 20e3] {
            assert_eq!(at(&mut v, rx), 100.1e6, "retune to {rx}");
        }
        // Past its edge: the window moves onto the receiver, and holds there.
        assert_eq!(at(&mut v, 100.6e6), 100.6e6);
        assert_eq!(at(&mut v, 100.62e6), 100.6e6);
        // Panned off the receiver, with the receiver still: stays panned.
        v.center_hz = 101.5e6;
        assert_eq!(at(&mut v, 100.62e6), 101.5e6);
        assert_eq!(at(&mut v, 100.62e6), 101.5e6);
        // Released: back on the receiver.
        v.hold = false;
        assert_eq!(at(&mut v, 100.62e6), 100.62e6);
    }

    /// The capability table the `/api/sdr/modes` endpoint serves must cover
    /// exactly the modes the server accepts, and each entry's serde name must
    /// be what the UI's `data-mode` buttons post back — the table is the
    /// contract between the two, so drift on either side fails here.
    #[test]
    fn mode_capabilities_cover_every_mode_and_match_the_wire_names() {
        let caps: Vec<ModeCapabilities> =
            SdrMode::all().iter().map(|m| m.capabilities()).collect();
        // Every variant of the enum is listed exactly once.
        let mut variants: Vec<SdrMode> = caps.iter().map(|c| c.mode).collect();
        variants.sort_by_key(|m| format!("{m:?}"));
        let mut all = SdrMode::all().to_vec();
        all.sort_by_key(|m| format!("{m:?}"));
        assert_eq!(variants, all, "SdrMode::all() must enumerate every mode once");

        // The serde names the UI posts to /api/sdr/mode.
        for c in &caps {
            let wire = serde_json::to_value(c.mode).unwrap();
            let name = wire.as_str().unwrap().to_string();
            assert!(
                !name.is_empty() && name == name.to_lowercase(),
                "{name:?}: mode wire name must be lowercase (the UI's data-mode values are)"
            );
            // Round-trip: the name the UI posts must deserialize back.
            let back: SdrMode = serde_json::from_value(wire).unwrap();
            assert_eq!(back, c.mode);
            // Scope kind must be one the binary frame's encoder knows.
            assert!(
                matches!(c.scope_kind, "audio" | "symbols" | "mpx"),
                "{name}: unknown scope kind {:?}",
                c.scope_kind
            );
            assert!(
                matches!(c.kind, "voice" | "data" | "voice+data"),
                "{name}: unknown capability kind {:?}",
                c.kind
            );
        }

        // Spot-check the drift-prone claims: NXDN is a real mode now, every
        // mode streams audio (the data modes carry their live channel), WFM
        // produces no decode events, and the digital voice modes draw symbol
        // scopes.
        let by = |m: SdrMode| caps.iter().find(|c| c.mode == m).unwrap().clone();
        assert_eq!(by(SdrMode::Nxdn).label, "NXDN");
        assert!(by(SdrMode::Nxdn).delivers_audio && by(SdrMode::Nxdn).delivers_decode_events);
        assert!(by(SdrMode::Pager).delivers_audio && by(SdrMode::Pager).delivers_decode_events);
        assert!(by(SdrMode::Flex).delivers_audio && by(SdrMode::Flex).delivers_decode_events);
        assert!(by(SdrMode::Wfm).delivers_audio && by(SdrMode::Wfm).delivers_decode_events);
        for m in [SdrMode::P25, SdrMode::Dmr, SdrMode::Nxdn, SdrMode::Flex] {
            assert_eq!(by(m).scope_kind, "symbols", "{m:?} must draw a symbol scope");
        }
        assert_eq!(by(SdrMode::Wfm).scope_kind, "mpx");
        assert_eq!(by(SdrMode::Nxdn).bandwidth_hz, NXDN_BANDWIDTH_HZ);
        // The recorder offers what the ring holds in each mode.
        assert_eq!(by(SdrMode::Nfm).capture, &["iq", "voice", "discriminator"]);
        for m in [SdrMode::Am, SdrMode::P25, SdrMode::Dmr, SdrMode::Nxdn, SdrMode::Scan] {
            assert_eq!(by(m).capture, &["iq"], "{m:?} captures channel I/Q only");
        }
        assert!(by(SdrMode::Wfm).capture.is_empty() && by(SdrMode::Pager).capture.is_empty());
    }

    /// The 80 m case that showed a dead band: tuner at 3.573 MHz, LO 100 kHz
    /// up at 3.673, zoomed 32x onto a receiver at 3.715.
    fn lo_clear_keeps_the_notch_out_of_a_zoomed_window() {
        let (rate, mode, zoom) = (8e6, SdrMode::Lsb, 32.0);
        let lo = lo_offset_for(rate, true, mode);
        assert_eq!(lo, 100e3);
        let view = 3.715e6;
        let f = lo_clear_freq(rate, true, mode, zoom, 3.573e6, view, &[view]).expect("the notch was in the window");
        let w = ShownWindow::new(rate, true, mode, zoom, f, view);
        let dc = f + lo;
        assert!((dc - w.shown_centre).abs() > w.shown_rate / 2.0 + DC_NOTCH_HZ, "DC {dc} still in {w:?}");
        // On the side it was on (below), and no nearer than the plan's offset.
        assert!(dc < view && view - dc >= lo, "DC {dc} for a receiver at {view}");
        // Once clear, it stays put: no retune on every block.
        assert_eq!(lo_clear_freq(rate, true, mode, zoom, f, view, &[view]), None);
        // Not zoomed, LO off, or nothing in the way: nothing moves.
        assert_eq!(lo_clear_freq(rate, true, mode, 1.0, 3.573e6, view, &[view]), None);
        assert_eq!(lo_clear_freq(rate, false, mode, zoom, 3.573e6, view, &[view]), None);
        // Even tuned dead on, the 100 kHz offset is inside a 244 kHz window:
        // zoomed at all, the notch was always in view. It moves out too.
        let f = lo_clear_freq(rate, true, mode, zoom, view, view, &[view]).expect("DC 100 kHz out is inside the window");
        assert!(f + lo - view > ShownWindow::new(rate, true, mode, zoom, f, view).shown_rate / 2.0 + DC_NOTCH_HZ);
        // A second receiver the move would leave outside the span stops it.
        assert_eq!(lo_clear_freq(rate, true, mode, 2.0, view - lo, view, &[view, view + 3.7e6]).is_some_and(|f| (view + 3.7e6 - f).abs() > 3.6e6), false);
        // At 2x DC cannot leave the window without pushing it into the
        // band edge (the 159.3 MHz DMR case): the tuner stays, centered.
        let (view, lo) = (159.309e6, lo_offset_for(rate, true, SdrMode::Dmr));
        assert_eq!(lo_clear_freq(rate, true, SdrMode::Dmr, 2.0, view - lo, view, &[view]), None);
        // Left at the edge by an earlier move, it goes back to the receiver.
        let pushed = view + 2.355e6 - lo;
        assert!(ShownWindow::new(rate, true, SdrMode::Dmr, 2.0, pushed, view).shown_centre - view > 0.2e6, "the old move clamped the window");
        assert_eq!(lo_clear_freq(rate, true, SdrMode::Dmr, 2.0, pushed, view, &[view]), Some(view - lo));
        let lo = lo_offset_for(rate, true, mode);
        // Every zoom from 2x: a window onto DC is moved off it, receivers kept.
        for z in [2.0, 4.0, 8.0, 16.0, 32.0] {
            let view = 7.074e6;
            let f = lo_clear_freq(rate, true, mode, z, view - lo, view, &[view]).unwrap_or(view - lo);
            let w = ShownWindow::new(rate, true, mode, z, f, view);
            assert!((w.shown_centre - view).abs() < 1.0, "{z}x: window not on the receiver");
            if z >= 4.0 {
                assert!((f + lo - w.shown_centre).abs() > w.shown_rate / 2.0, "{z}x: DC in the window");
            }
            assert!((view - f).abs() <= display_rate_for(rate, true, mode) * 0.45, "{z}x: receiver left the span");
        }
    }

    #[test]
    fn the_zero_if_lo_plan_keeps_dc_out_of_the_channel_and_most_of_the_span() {
        LO_PLAN_ZERO_IF.store(true, Ordering::Relaxed);
        for (mode, rate) in [(SdrMode::Nfm, 5e6), (SdrMode::Wfm, 5e6), (SdrMode::Usb, 2e6), (SdrMode::Nfm, 250e3)] {
            assert!(lo_active(rate, true, mode), "{mode:?} at {rate}");
            let off = lo_offset_for(rate, true, mode);
            let edge = f64::from(mode.bandwidth_hz()) / 2.0 + f64::from(mode.passband_offset_hz()).abs();
            assert!(off >= edge + 15e3, "{mode:?}: DC hump ({off} Hz) reaches the passband edge ({edge} Hz)");
            let keep = if rate >= 1e6 { 0.8 } else { 0.65 };
            assert!(display_rate_for(rate, true, mode) >= rate * keep, "{mode:?} at {rate} keeps most of the span");
        }
        assert_eq!(display_rate_for(5e6, true, SdrMode::Nfm), 4.8e6);
        assert_eq!(display_rate_for(5e6, true, SdrMode::Wfm), 4.7e6);
        // A span change moves it too once the 15% cap binds: 100 kHz at
        // 2 MS/s is 75 kHz at 500 kS/s, and WFM's 150 kHz no longer fits.
        assert_eq!(lo_move_between((2e6, SdrMode::Nfm), (8e6, SdrMode::Nfm), true), LoMove::Stay);
        assert_eq!(lo_move_between((2e6, SdrMode::Nfm), (500e3, SdrMode::Nfm), true), LoMove::Retune);
        assert_eq!(lo_move_between((2e6, SdrMode::Wfm), (500e3, SdrMode::Wfm), true), LoMove::Reopen);
        assert_eq!(lo_move_between((2e6, SdrMode::Nfm), (500e3, SdrMode::Nfm), false), LoMove::Stay);
        // NFM parks the LO 100 kHz out and WFM 150 kHz: a switch between
        // them moves the tuner, or the channel is mixed 50 kHz off.
        assert_eq!(lo_move(8e6, true, SdrMode::Nfm, SdrMode::Wfm), LoMove::Retune);
        assert_eq!(lo_move(8e6, true, SdrMode::Wfm, SdrMode::Am), LoMove::Retune);
        assert_eq!(lo_move(8e6, true, SdrMode::Nfm, SdrMode::Am), LoMove::Stay);
        assert_eq!(lo_move(8e6, true, SdrMode::Wfm, SdrMode::Scan), LoMove::Reopen);
        assert_eq!(lo_move(8e6, false, SdrMode::Nfm, SdrMode::Wfm), LoMove::Stay);
        // Run here, under the zero-IF plan this test holds.
        lo_clear_keeps_the_notch_out_of_a_zoomed_window();
        LO_PLAN_ZERO_IF.store(false, Ordering::Relaxed);
        assert_eq!(lo_clear_freq(8e6, true, SdrMode::Lsb, 32.0, 3.573e6, 3.673e6, &[3.673e6]), None, "the RTL plan's window never holds DC");
        // The RTL plan's offset is a fraction of the span, whatever the mode.
        assert_eq!(lo_move(2.4e6, true, SdrMode::Nfm, SdrMode::Usb), LoMove::Stay);
        assert_eq!(lo_move_between((2.4e6, SdrMode::Nfm), (1.2e6, SdrMode::Nfm), true), LoMove::Retune);
        // The RTL plan is unchanged: fs/8 offset, 20% of the span shown.
        assert_eq!(lo_offset_for(2.4e6, true, SdrMode::Nfm), 300e3);
        assert!((display_rate_for(2.4e6, true, SdrMode::Nfm) - 480e3).abs() < 1.0);
    }

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

    /// The scan slot's audio-conditioning chain must be rated for the rate of
    /// the audio it actually receives. Built for a nominal 8 kHz while the
    /// slot chain delivers span/round(span/48 kHz) (~47.6 kHz), the gate's
    /// sample-counted timers ran ~6× short in real time — the 400 ms-class
    /// close delay became ~67 ms — and the gate chopped marginal carriers
    /// into fragments, live and in the recordings. The tell is real-time
    /// behaviour: a hovering noise-band envelope (a weak carrier's partial
    /// quieting) must hold the gate open for the designed close delay, not a
    /// sixth of it.
    #[test]
    fn scan_slot_monitor_chain_is_rated_for_the_audio_it_receives() {
        let mut slot = build_scan_slot(
            145_500_000.0,
            2_048_000.0,
            146_000_000.0,
            &[ScanMode::Nfm],
        );
        let fs = slot.chain.fs_out() as f32;
        assert!(
            (fs - 2_048_000.0 / 43.0).abs() < 1.0,
            "chain rate {fs} is not the span's real decimation output"
        );
        // Open the gate on a fully quieted carrier (noise band pinned low)…
        let quiet = vec![0.0f32; fs as usize];
        let mut voice = vec![0.3f32; quiet.len()];
        slot.mon_gate.process(&mut voice, &quiet);
        assert!(slot.mon_gate.is_open(), "gate never opened on a quieted carrier");
        // …then feed the marginal-carrier envelope: hovering just over the
        // close line. The depth-scaled close delay for 1.2× is ~0.36 s, so
        // 250 ms of hovering must not close it. At the old 8 kHz rating the
        // same hover closed the gate after ~67 ms.
        let hover_len = (fs * 0.25) as usize;
        let hover = vec![scannerd_engine::noisegate::DEFAULT_CLOSE * 1.2; hover_len];
        let mut voice = vec![0.3f32; hover_len];
        slot.mon_gate.process(&mut voice, &hover);
        assert!(
            slot.mon_gate.is_open(),
            "gate closed inside the designed close delay — monitor chain is mis-rated again"
        );
    }


    /// format gained a binary path (b0f7411) this field was silently dropped
    /// from `encode_fft_frame` and hardcoded to `null` in the browser's
    /// decoder, and the live classifier panel went dark for two days while
    /// every unit test kept passing. This pins both ends of the wire.
    #[test]
    fn fft_binary_frame_carries_the_classification() {
        let mut ev = SdrEvent::Fft {
            center_hz: 155_100_000.0,
            rate_hz: 1_048_576.0,
            pwr: vec![0.0; 64],
            max_hold: vec![0.0; 64],
            wf_peak: Vec::new(),
            wf_row: Vec::new(),
            peak_iq: 0.25,
            inspect_hz: 155_000_000.0,
            classification: Some(ClassificationResult {
                active: true,
                snr_db: 11.5,
                rf_dbfs: -42.0,
                peak_dev_hz: 4_800.0,
                rms_dev_hz: 2_400.0,
                center_offset_hz: -310.0,
                modulation: "C4FM @ 4800 Bd".into(),
                protocol: "P25 Phase 1".into(),
                details: Some("NAC: $293 · LDU1".into()),
                confidence: 0.98,
            }),
            peaks: Vec::new(),
            scope: vec![12, -34, 56],
            scope_rate_hz: 48_000.0,
            scope_kind: "symbols",
            symbol_rate_hz: 4800.0,
            channel_dbfs: -40.0,
            noise_dbfs: -51.5,
        };

        // A Some classification survives encoding...
        let buf = encode_fft_frame(&ev).unwrap();
        assert_eq!(buf[0], FFT_FRAME_MAGIC);

        // ...and the same event with None keeps the frame decodable rather
        // than desynchronising the reader.
        if let SdrEvent::Fft { classification, .. } = &mut ev {
            *classification = None;
        }
        let buf_none = encode_fft_frame(&ev).unwrap();

        // v3 appends exactly one trailing block, so a present classification
        // must be strictly larger than an absent one.
        assert!(
            buf.len() > buf_none.len(),
            "a present classification must add bytes to the frame"
        );

        // Full manual decode mirroring onSdrFftBinary, so any encoder/client
        // drift fails here first:
        let read_classification = |buf: &[u8]| -> Option<ClassificationResult> {
            let mut off = 1usize;
            let mut f64le = |off: &mut usize| -> f64 {
                let v = f64::from_le_bytes(buf[*off..*off + 8].try_into().unwrap());
                *off += 8;
                v
            };
            let mut f32le = |off: &mut usize| -> f32 {
                let v = f32::from_le_bytes(buf[*off..*off + 4].try_into().unwrap());
                *off += 4;
                v
            };
            let mut u32le = |off: &mut usize| -> u32 {
                let v = u32::from_le_bytes(buf[*off..*off + 4].try_into().unwrap());
                *off += 4;
                v
            };
            let _center = f64le(&mut off);
            let _rate = f64le(&mut off);
            let _peak_iq = f32le(&mut off);
            let _inspect = f64le(&mut off);
            off += 2 + 1 + 2; // scope rate u16, kind u8, symbol rate u16
            let _ch = f32le(&mut off);
            let _noise = f32le(&mut off);
            let scope_len = u32le(&mut off) as usize;
            off += scope_len * 2;
            let pwr_len = u32le(&mut off) as usize;
            let mh_len = u32le(&mut off) as usize;
            let peaks_len = u32le(&mut off) as usize;
            off += (pwr_len + mh_len) * 4;
            for _ in 0..peaks_len {
                off += 8 + 4;
                let label_len = buf[off] as usize;
                off += 1 + label_len;
            }
            if off >= buf.len() || buf[off] == 0 {
                return None;
            }
            off += 1;
            let active = buf[off] != 0;
            off += 1;
            let snr_db = f32le(&mut off);
            let rf_dbfs = f32le(&mut off);
            let peak_dev_hz = f32le(&mut off);
            let rms_dev_hz = f32le(&mut off);
            let center_offset_hz = f32le(&mut off);
            let confidence = f32le(&mut off);
            let mut get = |off: &mut usize| -> String {
                let len = buf[*off] as usize;
                *off += 1;
                let s = String::from_utf8(buf[*off..*off + len].to_vec()).unwrap();
                *off += len;
                s
            };
            let protocol = get(&mut off);
            let modulation = get(&mut off);
            let details = get(&mut off);
            Some(ClassificationResult {
                active,
                snr_db,
                rf_dbfs,
                peak_dev_hz,
                rms_dev_hz,
                center_offset_hz,
                modulation,
                protocol,
                details: (!details.is_empty()).then_some(details),
                confidence,
            })
        };

        let c = read_classification(&buf).expect("classification missing from v3 frame");
        assert!(c.active);
        assert_eq!(c.protocol, "P25 Phase 1");
        assert_eq!(c.modulation, "C4FM @ 4800 Bd");
        assert_eq!(c.details.as_deref(), Some("NAC: $293 · LDU1"));
        assert!((c.snr_db - 11.5).abs() < 1e-5);
        assert!((c.confidence - 0.98).abs() < 1e-5);

        let n = read_classification(&buf_none);
        assert!(n.is_none(), "None classification must encode as absent");

        // And the JSON path (used by replay and older clients) still carries it.
        if let SdrEvent::Fft { classification, .. } = &mut ev {
            *classification = Some(ClassificationResult {
                active: true,
                snr_db: 11.5,
                rf_dbfs: -42.0,
                peak_dev_hz: 4_800.0,
                rms_dev_hz: 2_400.0,
                center_offset_hz: -310.0,
                modulation: "C4FM @ 4800 Bd".into(),
                protocol: "P25 Phase 1".into(),
                details: Some("NAC: $293 · LDU1".into()),
                confidence: 0.98,
            });
        }
        let json = serde_json::to_string(&ev).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["classification"]["protocol"], "P25 Phase 1");
        assert_eq!(parsed["classification"]["details"], "NAC: $293 · LDU1");
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

    /// The digital voice receivers ride a narrowband chain's output instead
    /// of extracting their own channel from the span (see
    /// `P25ChannelReceiver::new_on_channel`). That costs no symbol-timing
    /// drift only because the channelized chain never decimates: its input
    /// is the span chain's `fs_out`, which must round to CHANNEL_RATE just
    /// once at every span the API accepts, leaving `fs_out` on exactly the
    /// grid a span-rate build would have produced. Pin that for the whole
    /// accepted range — UI rates plus the half-integer boundaries where
    /// `round()` changes its mind.
    #[test]
    fn channelized_receivers_keep_the_span_rate_grid() {
        let rates = [
            200_000.0, 216_000.0, 250_000.0, 320_000.0, 500_000.0, 504_000.0, 1_024_000.0,
            1_536_000.0, 2_048_000.0, 2_400_000.0, 3_200_000.0,
        ];
        for &rate in &rates {
            let span_chain = DecodeChain::new(rate, INSPECT_BANDWIDTH_HZ, INSPECT_RATE);
            let fs = span_chain.fs_out();
            for (bw, channel_rate, which) in [
                (C4FM_BANDWIDTH_HZ, scannerd_engine::p25::CHANNEL_RATE, "P25"),
                (DMR_BANDWIDTH_HZ, scannerd_engine::dmr::CHANNEL_RATE, "DMR"),
                (
                    NXDN_BANDWIDTH_HZ,
                    scannerd_engine::nxdn::CHANNEL_RATE,
                    "NXDN",
                ),
            ] {
                let rx = DecodeChain::new(fs, bw, channel_rate);
                assert_eq!(
                    rx.fs_out(),
                    fs,
                    "{which} at span {rate}: channelized chain must not resample"
                );
                assert_eq!(
                    rx.fs_out(),
                    DecodeChain::new(rate, bw, channel_rate).fs_out(),
                    "{which} at span {rate}: grid must match a span-rate build"
                );
            }
        }
    }
}
