//! One SDR device, owned by one worker thread.
//!
//! Adapted from `hfscan/src/radio.rs`. The actor shape is unchanged — commands
//! in, events and logs out, the device touched from exactly one thread — and
//! the capability discovery is kept verbatim because it already handles the
//! RSP1A's gain-*reduction* semantics alongside the RTL-SDR's aggregate dB gain,
//! and this host has both.
//!
//! Three things are new. Devices are addressed by **serial** rather than by a
//! bare index, because index order is not stable across replug and this host has
//! three otherwise-identical dongles. Samples leave through a [`Fanout`] rather
//! than a single channel. And the analog filter width is configured explicitly
//! instead of being derived from an amateur band plan, since a scanner's spans
//! are set by the system being monitored, not by band edges.
//!
//! A device is either a local RTL-SDR or an SDRplay receiver on another host,
//! reached through SoapyRemote (`driver=remote`). SoapyRemote proxies the
//! whole Soapy API — gain elements, settings and the stream — so nothing above
//! this layer knows the difference beyond [`DeviceConfig::remote`] naming the
//! host. Remote is SDRplay only on purpose: an RTL-SDR's I2C faults are only
//! visible on the stderr of the process that owns it (see `driver_log`), and
//! over the network that is the server, not this one.

use crate::fanout::{Fanout, IqBlock};
use anyhow::{Context, Result};
use num_complex::Complex32;
use soapysdr::Direction::Rx;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError, channel, sync_channel};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq)]
pub enum GainControl {
    /// A conventional aggregate gain value in dB (RTL-SDR and most others).
    Overall { min: f64, max: f64 },
    /// SDRplay's two gain-*reduction* controls. Larger values mean less gain.
    Sdrplay {
        rfgr_min: f64,
        rfgr_max: f64,
        ifgr_min: f64,
        ifgr_max: f64,
    },
}

#[derive(Clone, Debug)]
pub struct Capabilities {
    pub driver: String,
    pub hardware: String,
    pub gain: GainControl,
    pub hardware_agc: bool,
    pub agc_setpoint: bool,
    pub rf_notch: bool,
    pub dab_notch: bool,
    pub iq_correction: bool,
    pub ppm: bool,
    /// The antenna-port bias-T (`biasT_ctrl`), SDRplay only.
    pub bias_t: bool,
    /// Discrete analog IF filter widths the driver offers, narrowest first.
    /// Empty where it offers a continuous range or none.
    pub if_bandwidths: Vec<f64>,
    /// Sample rates the driver accepts, as (min, max) pairs; a discrete rate
    /// is a pair with both ends equal.
    pub rate_ranges: Vec<(f64, f64)>,
    /// Lowest and highest frequency the tuner reaches.
    pub freq_range: Option<(f64, f64)>,
}

impl Capabilities {
    /// Whether the driver accepts `rate` exactly, or within one of its ranges.
    pub fn supports_rate(&self, rate: f64) -> bool {
        self.rate_ranges
            .iter()
            .any(|&(lo, hi)| rate >= lo - 0.5 && rate <= hi + 0.5)
    }
}

#[derive(Clone, Debug, Default)]
pub struct State {
    /// Gain in dB on the scale `Cmd::Gain` takes. For SDRplay that is the
    /// combined LNA and IF gain (see [`SdrplayGain`]), not the driver's own
    /// "overall" value, which runs the other way.
    pub overall_gain: Option<f64>,
    /// The range `Cmd::Gain` is clamped to. Constant on an RTL-SDR; on an
    /// SDRplay it follows the band, because the LNA's reductions do.
    pub gain_range: Option<(f64, f64)>,
    pub rfgr: Option<f64>,
    pub ifgr: Option<f64>,
    pub agc: Option<bool>,
    pub ppm: Option<f64>,
    pub rate: Option<f64>,
    pub bandwidth: Option<f64>,
    pub frequency: Option<f64>,
    /// Front-end switches, read back from the driver. `None` where the
    /// device has no such switch.
    pub bias_t: Option<bool>,
    pub rf_notch: Option<bool>,
    pub dab_notch: Option<bool>,
    /// SDRplay IF AGC target, dBFS, and the RSP's own I/Q correction, as
    /// read back from the driver.
    pub agc_setpoint: Option<f64>,
    pub iq_correction: Option<bool>,
    /// The analog IF filter was set by hand rather than from the span.
    pub bandwidth_manual: bool,
    /// SDRplay: the LNA reductions this band has, dB, one per state used
    /// (empty where not known), and whether the LNA is stepping on its own
    /// under the RSP's IF AGC.
    pub lna_steps_db: Vec<f64>,
    pub lna_auto: bool,
    /// SDRplay: the API's power-overload report, now and how many times
    /// since the device opened. `None` when the driver does not pass it on.
    pub overload: Option<bool>,
    pub overload_count: Option<u64>,
}

/// LNA state, IF gain reduction and the API's overload report, without the
/// rest of [`State`], whose frequency readback the server acts on.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GainReadout {
    /// The combined gain on `Cmd::Gain`'s scale, from the live readings.
    pub overall: Option<f64>,
    pub rfgr: Option<f64>,
    pub ifgr: Option<f64>,
    pub overload: Option<bool>,
    pub overload_count: Option<u64>,
}

#[derive(Clone, Debug)]
pub enum Event {
    Capabilities(Capabilities),
    State(State),
    /// An SDRplay's gain as it stands, sent each second it changes: under
    /// the RSP's IF AGC the IF moves on its own and the LNA follows it.
    Gain(GainReadout),
    StreamStats {
        /// Blocks the device produced that no subscriber had room for.
        dropped_blocks: u64,
        /// Subscriber-block deliveries missed because that consumer was behind.
        lagged_deliveries: u64,
        clipped_fraction: f64,
    },
}

pub enum Cmd {
    Tune(f64),
    Gain(f64),
    /// With automatic gain selected, hand the tuner its own AGC instead of
    /// the guarded loop. See the handler for why anyone would.
    TunerAgc(bool),
    Rfgr(f64),
    Ifgr(f64),
    Agc(bool),
    BiasT(bool),
    /// SDRplay's AM/FM broadcast band-stop filters.
    RfNotch(bool),
    /// SDRplay's DAB band-stop filter.
    DabNotch(bool),
    /// SDRplay IF AGC target level, dBFS (-60..0). Only acts while the IF
    /// AGC is running.
    AgcSetpoint(f64),
    /// SDRplay's own I/Q imbalance correction (`iqcorr_ctrl`).
    IqCorrection(bool),
    /// Analog IF filter width, Hz; `None` returns to choosing it from the span.
    IfBandwidth(Option<f64>),
    Ppm(f64),
    Rate(f64),
    /// Enable or disable the automatic clip guard.
    ///
    /// The guard walks the tuner gain down when the ADC clips and back up when
    /// it clears. That is right for an unattended scanner, and wrong for an
    /// operator who has just set a gain by hand and expects it to stay there.
    ClipGuard(bool),
    Quit,
}

/// What a device is currently being used for. Roles are advisory — the manager
/// uses them to pick a device for a job and to explain itself in the UI.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum Role {
    #[default]
    Idle,
    /// Conventional analog channels within one tuned span.
    Conventional,
    /// A trunked system's control channel.
    TrunkControl,
    /// A trunked system's voice channels.
    TrunkVoice,
    /// 1090 MHz Mode S / ADS-B.
    AdsB,
    /// 978 MHz UAT.
    Uat,
    /// Dedicated 144.390 MHz APRS.
    Aprs,
    /// Hopping UAT / SAME / pager stick.
    DataScan,
    /// Live spectrum and waterfall analyzer.
    Sdr,
}

#[derive(Clone, Debug)]
pub struct DeviceConfig {
    /// Serial as reported by the driver, e.g. `"00000003"`.
    pub serial: String,
    pub rate: f64,
    pub freq: f64,
    /// Aggregate gain in dB. `None` leaves hardware AGC enabled.
    pub gain: Option<f64>,
    /// Frequency correction. Measured per device — see the `ppm-cal` tool.
    pub ppm: f64,
    /// Width that must stay flat, used to pick the analog IF filter. Zero means
    /// "no requirement", in which case the span itself is the only constraint.
    pub cover_hz: f64,
    pub role: Role,
    /// SoapyRemote URL of the host the device is on (`tcp://172.16.0.216:55132`),
    /// or `None` for a radio on this machine's USB.
    pub remote: Option<String>,
    /// The Soapy driver that owns the radio: `rtlsdr` or `sdrplay`.
    pub driver: &'static str,
    /// Power the antenna port. Off unless asked for: DC on a feedline that
    /// does not expect it is the wrong default. On where the feedline has a
    /// preamp that needs it, or the receiver is deaf.
    pub bias_t: bool,
    /// SDRplay band-stop filters; `None` leaves the driver's setting alone.
    pub rf_notch: Option<bool>,
    pub dab_notch: Option<bool>,
}

impl DeviceConfig {
    pub fn new(serial: impl Into<String>, freq: f64, rate: f64) -> Self {
        Self {
            serial: serial.into(),
            rate,
            freq,
            gain: None,
            ppm: 0.0,
            cover_hz: 0.0,
            role: Role::Idle,
            remote: None,
            driver: RTLSDR,
            bias_t: false,
            rf_notch: None,
            dab_notch: None,
        }
    }

    /// The Soapy device string this config addresses.
    ///
    /// A remote device streams over TCP. SoapyRemote's default is UDP with
    /// no retransmission, so a lost datagram is lost samples, and through a
    /// tunnel with a small MTU (a tailnet's is 1280 bytes) every default-sized
    /// datagram is also split in two. TCP on a LAN-latency path costs nothing
    /// measurable and keeps the sample timeline whole: 10 MS/s ran with no
    /// lost samples.
    pub fn soapy_args(&self) -> String {
        match &self.remote {
            None => format!("driver={},serial={}", self.driver, self.serial),
            Some(url) => format!(
                "driver=remote,remote={url},remote:driver=sdrplay,remote:prot=tcp,serial={}",
                self.serial
            ),
        }
    }
}

/// The Soapy driver keys this server opens radios through.
pub const RTLSDR: &str = "rtlsdr";
pub const SDRPLAY: &str = "sdrplay";

/// SoapyRemote's default port.
pub const REMOTE_PORT: u16 = 55132;

/// A SoapyRemote URL from what an operator would type: `rsp`,
/// `rsp:55132`, or a full `tcp://rsp:55132`.
pub fn remote_url(spec: &str) -> String {
    let spec = spec.trim();
    if spec.contains("://") {
        return spec.to_string();
    }
    // A bracketed IPv6 literal carries its own colons; only a colon after
    // the closing bracket (or in a plain host) is a port.
    let host_end = spec.rfind(']').map_or(0, |i| i + 1);
    if spec[host_end..].contains(':') {
        format!("tcp://{spec}")
    } else {
        format!("tcp://{spec}:{REMOTE_PORT}")
    }
}

/// The host part of a SoapyRemote URL, for labels.
pub fn remote_host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    match rest.rfind(':') {
        Some(i) if !rest[i..].contains(']') => &rest[..i],
        _ => rest,
    }
}

/// Analog IF width that must stay flat so every `freq` around `center` is
/// inside the tuner filter. One channel-bandwidth of margin keeps the
/// outermost carrier off the analog skirt.
///
/// This is what should be written to [`DeviceConfig::cover_hz`]. Asking for
/// the whole sample rate instead (the old conventional-scanner default)
/// selects a 2 MHz R820T filter to decode a 30 kHz VHF cluster, and the
/// extra energy eats the 8-bit ADC's dynamic range.
pub fn cover_hz(center: f64, freqs: impl IntoIterator<Item = f64>, channel_bw_hz: f64) -> f64 {
    let max_off = freqs
        .into_iter()
        .map(|f| (f - center).abs())
        .fold(0.0_f64, f64::max);
    // Extra 80 kHz so the outermost channel is not sitting on the analog
    // skirt. Continuous-range tuners (E4000) honour `cover_hz` exactly.
    (2.0 * max_off + channel_bw_hz.max(0.0) + 80_000.0).max(channel_bw_hz.max(0.0))
}

/// Analog IF width for a single channel offset-tuned by `offset_hz` from the
/// LO — the P25 control and voice path.
///
/// The extra 200 kHz is load-bearing on a continuous-range tuner: without
/// it, `2*offset + bw` parks the carrier on the analog edge.
pub fn cover_hz_offset(offset_hz: f64, channel_bw_hz: f64) -> f64 {
    2.0 * offset_hz.abs() + channel_bw_hz.max(0.0) + 200_000.0
}

/// Backs the configured gain off when the ADC is clipping, then creeps it
/// back toward that ceiling once the overload is gone.
///
/// RTL-SDR is 8 bits: a preamp or a loud neighbour that slams the converter
/// raises the floor under every weak signal in the span. The radio thread
/// already measures clip fraction; this is the loop that acts on it. It
/// never exceeds the operator's requested gain, and it never touches
/// hardware AGC or SDRplay's two-element controls.
/// The R820T/R828D tuner's gain settings, in dB. librtlsdr rounds any
/// request down to one of these, so a loop stepping in decibels writes
/// values the tuner cannot take and the readback drifts from the hardware.
/// Every gain this crate writes to such a tuner is snapped to the table.
const R82XX_GAINS_DB: &[f64] = &[
    0.0, 0.9, 1.4, 2.7, 3.7, 7.7, 8.7, 12.5, 14.4, 15.7, 16.6, 19.7, 20.7, 22.9, 25.4, 28.0,
    29.7, 32.8, 33.8, 36.4, 37.2, 38.6, 40.2, 42.1, 43.4, 43.9, 44.5, 48.0, 49.6,
];

/// Discrete gain settings for this tuner, empty when it is continuous.
fn gain_steps(caps: &Capabilities) -> &'static [f64] {
    let hw = caps.hardware.to_ascii_uppercase();
    if hw.contains("R820") || hw.contains("R828") {
        R82XX_GAINS_DB
    } else {
        &[]
    }
}

/// The table value nearest to `g` (continuous tuners return `g`).
fn snap_gain(steps: &[f64], g: f64) -> f64 {
    steps
        .iter()
        .copied()
        .min_by(|a, b| (a - g).abs().total_cmp(&(b - g).abs()))
        .unwrap_or(g)
}

/// Smallest table value at least `g` (or the largest if none), for
/// stepping up; `g` itself on a continuous tuner.
fn snap_gain_up(steps: &[f64], g: f64) -> f64 {
    steps
        .iter()
        .copied()
        .find(|v| *v >= g - 1e-6)
        .or_else(|| steps.last().copied())
        .unwrap_or(g)
}

/// Largest table value at most `g` (or the smallest if none), for
/// stepping down.
fn snap_gain_down(steps: &[f64], g: f64) -> f64 {
    steps
        .iter()
        .rev()
        .copied()
        .find(|v| *v <= g + 1e-6)
        .or_else(|| steps.first().copied())
        .unwrap_or(g)
}

struct ClipGuard {
    ceiling: f64,
    current: f64,
    min: f64,
    max: f64,
    high: u8,
    low: u8,
    steps: &'static [f64],
}

impl ClipGuard {
    fn new(gain: f64, min: f64, max: f64, steps: &'static [f64]) -> Self {
        let g = snap_gain(steps, gain.clamp(min, max));
        Self {
            ceiling: g,
            current: g,
            min,
            max,
            high: 0,
            low: 0,
            steps,
        }
    }

    /// Guard for automatic gain: starts at `start` and may climb all the
    /// way to the hardware maximum on a quiet band, walking back down as
    /// soon as the ADC clips.
    fn auto(start: f64, min: f64, max: f64, steps: &'static [f64]) -> Self {
        let mut g = Self::new(start, min, max, steps);
        g.ceiling = max;
        g
    }

    /// One observation per second: the fraction of samples that clipped
    /// and the largest |I| or |Q| seen. Returns a new gain when the
    /// hardware should be updated.
    ///
    /// Coming down is always 2 dB after two clipped seconds. Going up
    /// depends on headroom: with the peak below a quarter of full scale
    /// (12 dB of room) the band is plainly quiet and the loop steps 3 dB
    /// every two seconds, so a weak-signal band reaches full sensitivity
    /// in well under a minute rather than the four it took at 1 dB per
    /// eight seconds; within that last 12 dB it creeps 1 dB per eight
    /// quiet seconds so it settles just under the clip point.
    fn on_stats(&mut self, clipped_fraction: f64, peak: f64) -> Option<f64> {
        const CLIP_HIGH: f64 = 0.001;
        const CLIP_LOW: f64 = 0.0001;
        const HIGH_NEED: u8 = 2;
        const LOW_NEED: u8 = 8;
        const FAST_NEED: u8 = 2;
        const FAST_PEAK: f64 = 0.25;
        /// No climbing at all above this peak: the next table step is up
        /// to 4 dB, and from a peak of 0.65 that lands on full scale. The
        /// loop used to hunt 8.7 → 12.5 → 14.4 (clip) → 8.7 every 16 s.
        const CLIMB_PEAK_MAX: f64 = 0.5;
        const DOWN_DB: f64 = 2.0;
        const UP_DB: f64 = 1.0;
        const FAST_UP_DB: f64 = 3.0;

        // A peak at full scale is clipping whatever the fraction says: a
        // spectrum instrument cares about the intermod one strong burst
        // makes, and at 0.1% of samples the loop sat clipping at 19.7 dB
        // on a busy VHF band for half a minute.
        if clipped_fraction > CLIP_HIGH || peak >= 0.98 {
            self.high = self.high.saturating_add(1);
            self.low = 0;
            if self.high >= HIGH_NEED {
                self.high = 0;
                let next = snap_gain_down(self.steps, self.current - DOWN_DB).max(self.min);
                if (next - self.current).abs() > 0.05 {
                    self.current = next;
                    return Some(self.current);
                }
            }
        } else if clipped_fraction < CLIP_LOW
            && peak < CLIMB_PEAK_MAX
            && self.current < self.ceiling - 0.05
        {
            self.low = self.low.saturating_add(1);
            self.high = 0;
            let (need, step) = if peak < FAST_PEAK { (FAST_NEED, FAST_UP_DB) } else { (LOW_NEED, UP_DB) };
            if self.low >= need {
                self.low = 0;
                let next = snap_gain_up(self.steps, self.current + step)
                    .min(self.ceiling)
                    .min(self.max);
                let next = snap_gain_down(self.steps, next).max(self.current);
                if (next - self.current).abs() > 0.05 {
                    self.current = next;
                    return Some(self.current);
                }
            }
        } else {
            self.high = 0;
            self.low = 0;
        }
        None
    }
}

/// A device the driver can see, before it is opened.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub serial: String,
    pub label: String,
    pub tuner: String,
    /// SoapyRemote URL when the device is on another host.
    pub remote: Option<String>,
    /// [`RTLSDR`] or [`SDRPLAY`].
    pub driver: &'static str,
}

/// Enumerate attached RTL-SDR dongles and SDRplay receivers, then the SDRplay
/// receivers on each SoapyRemote host in `remotes` (`172.16.0.216`,
/// `rsp:55132`, or a URL).
///
/// Pinned to `driver=rtlsdr` on purpose: the osmosdr Soapy module is also
/// installed on this host and will happily claim the same hardware, which makes
/// enumeration order and device count depend on which module answers first.
/// Remote enumeration is pinned to `driver=sdrplay` for the same reason and
/// one more: an unfiltered find makes the server probe every module it has,
/// network scans included, which takes seconds instead of milliseconds.
///
/// A remote that does not answer is skipped, not an error: the local radios
/// are still worth listing while another host is down. So is a local SDRplay
/// search that fails, as it does where the SDRplay module or its API service
/// is missing.
pub fn enumerate(remotes: &[String]) -> Result<Vec<DeviceInfo>> {
    let found = soapysdr::enumerate("driver=rtlsdr").context("enumerating rtlsdr devices")?;
    let mut out: Vec<DeviceInfo> = found
        .iter()
        .map(|a| DeviceInfo {
            serial: a.get("serial").unwrap_or_default().to_string(),
            label: a.get("label").unwrap_or_default().to_string(),
            tuner: a.get("tuner").unwrap_or_default().to_string(),
            remote: None,
            driver: RTLSDR,
        })
        .collect();
    // The RSP on this host's own USB, as where the server runs beside it.
    if let Ok(found) = soapysdr::enumerate("driver=sdrplay") {
        out.extend(found.iter().map(|a| sdrplay_info(a, None)));
    }
    for spec in remotes {
        let url = remote_url(spec);
        let Ok(found) =
            soapysdr::enumerate(format!("driver=remote,remote={url},remote:driver=sdrplay").as_str())
        else {
            continue;
        };
        out.extend(found.iter().map(|a| sdrplay_info(a, Some(url.clone()))));
    }
    Ok(out)
}

fn sdrplay_info(a: &soapysdr::Args, remote: Option<String>) -> DeviceInfo {
    let label = a.get("label").unwrap_or_default().to_string();
    DeviceInfo {
        serial: a.get("serial").unwrap_or_default().to_string(),
        tuner: sdrplay_model(&label).to_string(),
        label,
        remote,
        driver: SDRPLAY,
    }
}

/// The model in an SDRplay enumeration label (`SDRplay Dev0 RSP1A 210304DD98`
/// → `RSP1A`), which is the closest thing the driver has to a tuner name.
fn sdrplay_model(label: &str) -> &str {
    label
        .split_whitespace()
        .find(|w| w.starts_with("RSP"))
        .unwrap_or("SDRplay")
}

pub struct Device {
    pub serial: String,
    pub cmd: Sender<Cmd>,
    pub log: Receiver<String>,
    pub events: Receiver<Event>,
    /// Subscribe here for samples. Cloneable and safe to share across threads.
    pub iq: Fanout,
    /// Rate the driver actually selected; some backends clamp the request.
    pub rate: f64,
    pub role: Role,
    /// What the driver said it can do, read once at open.
    pub caps: Capabilities,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Device {
    fn drop(&mut self) {
        let _ = self.cmd.send(Cmd::Quit);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

const MAX_BLOCK: usize = 131072;

/// The largest of |I| and |Q| over a block, and how many samples have
/// either at 0.98 of full scale or over.
///
/// Eight samples abreast, each lane with its own running peak and count:
/// one peak carried sample to sample is a chain every compare waits on,
/// and at 8 MS/s that scan was a twentieth of this thread.
fn peak_and_clips(iq: &[Complex32]) -> (f32, u64) {
    let mut peak = [0.0f32; 8];
    let mut clips = [0u32; 8];
    let mut total = 0u64;
    // Counted in u32 lanes, emptied well before one could overflow.
    for run in iq.chunks(8 << 20) {
        let chunks = run.chunks_exact(8);
        let rest = chunks.remainder();
        for c in chunks {
            for l in 0..8 {
                let (re, im) = (c[l].re.abs(), c[l].im.abs());
                let m = if re > im { re } else { im };
                if m > peak[l] {
                    peak[l] = m;
                }
                clips[l] += u32::from(m > 0.98);
            }
        }
        for c in rest {
            let (re, im) = (c.re.abs(), c.im.abs());
            let m = if re > im { re } else { im };
            if m > peak[0] {
                peak[0] = m;
            }
            clips[0] += u32::from(m > 0.98);
        }
        total += clips.iter().map(|&c| u64::from(c)).sum::<u64>();
        clips = [0; 8];
    }
    (peak.iter().copied().fold(0.0, f32::max), total)
}

/// Delivery interval every block is gathered to. The RTL path gets it from
/// the driver's USB buffer size; anything else is accumulated to it, because
/// SoapySDRPlay3 hands over one ~1000-sample packet per read and SoapyRemote
/// one network frame (1006 samples over TCP). At 2 MS/s that is two
/// thousand blocks a second, and a 32-deep subscriber queue sized for 20 ms
/// blocks would then hold 16 ms — one scheduling hiccup from dropping.
const BLOCK_SECONDS: f64 = 0.020;

/// Samples per delivery: `BLOCK_SECONDS` of them, so two deliveries are the
/// 40 ms a display frame spans.
///
/// Past `MAX_BLOCK` that no longer fits, and a delivery capped there cut
/// the frame's time unevenly: at 8 MS/s the cap is 16.4 ms, so spectrum
/// frames went out after two deliveries and after three by turns, 33 and
/// 49 ms apart, each averaged over half as much signal again as the last,
/// with the spectrum's segment budget planned for the wrong one every
/// time. There a delivery is a whole fraction of the 40 ms instead (a
/// third at 8 MS/s, a quarter at 10), in whole 4096-sample blocks so the
/// image corrector's blocks still fall on its edges.
fn block_for(rate: f64) -> usize {
    let want = (rate * BLOCK_SECONDS).round() as usize;
    if want <= MAX_BLOCK {
        return want.max(1024);
    }
    let frame = rate * BLOCK_SECONDS * 2.0;
    let parts = (frame / MAX_BLOCK as f64).ceil();
    (((frame / parts / 4096.0).round() as usize).max(1) * 4096).min(MAX_BLOCK)
}

/// Opens the stream and returns it with the block size to gather per
/// delivery.
fn rx_stream(
    dev: &soapysdr::Device,
    driver: &str,
    rate: f64,
    remote: bool,
    log_tx: Option<&SyncSender<String>>,
) -> Result<(soapysdr::RxStream<Complex32>, usize)> {
    let mut args = soapysdr::Args::new();
    let mut block = block_for(rate);
    if remote {
        // Over the wire as the RSP's own 16-bit samples, converted to float
        // here: half the bytes of CF32 and none of the 14-bit ADC's range
        // lost, which CS8 would throw away.
        args.set("remote:format", "CS16");
    }
    if driver.to_ascii_lowercase().contains("rtlsdr") {
        // Target ~20 ms delivery interval (50 Hz) so userspace receives
        // continuous, low-latency sample blocks across all sample rates
        // instead of the default 131k-sample bursts (which cause 500+ ms
        // stutter at low sample rates).
        let target_samples = (rate * 0.020).round() as usize;
        let target_bytes = target_samples * 2; // 2 bytes per complex sample in CS8
        // USB bulk endpoint packet size is 512 bytes; librtlsdr requires bufflen % 512 == 0.
        let bufflen = ((target_bytes + 256) / 512 * 512).clamp(4096, 131072);
        args.set("bufflen", bufflen.to_string());
        // Maintain a deep ring of buffers (48 buffers * ~20ms = ~960ms queue)
        // and 32 async USB transfers to prevent any underruns or dropped packets.
        args.set("buffers", "48");
        args.set("asyncBuffs", "32");
        // One USB buffer per read, exactly as before blocks were gathered:
        // asking for precisely a buffer's worth never leaves a remainder
        // behind to split the next one.
        block = bufflen / 2;
        if let Some(log) = log_tx {
            let samples = bufflen / 2;
            let ms = (samples as f64 / rate) * 1000.0;
            let _ = log.try_send(format!(
                "rtlsdr stream config: bufflen={bufflen} ({samples} samples, {ms:.1} ms), buffers=48, asyncBuffs=32"
            ));
        }
    }
    let stream = dev
        .rx_stream_args::<Complex32, _>(&[0], args)
        .map_err(|e| anyhow::anyhow!("rx stream: {e}"))?;
    Ok((stream, block))
}

pub fn open(cfg: &DeviceConfig) -> Result<Device> {
    let (cmd_tx, cmd_rx) = channel::<Cmd>();
    let (log_tx, log_rx) = sync_channel::<String>(64);
    let (event_tx, event_rx) = sync_channel::<Event>(32);
    let fanout = Fanout::new();

    // Open on this thread so startup errors surface to the caller rather than
    // vanishing into a worker that has already been reported as running.
    // Retry briefly with backoff to accommodate USB release settlement.
    let args = cfg.soapy_args();
    let mut last_err = None;
    let mut dev = None;
    for attempt in 0..4 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(50 * (1 << (attempt - 1))));
        }
        match soapysdr::Device::new(args.as_str()) {
            Ok(d) => {
                dev = Some(d);
                break;
            }
            Err(e) => {
                last_err = Some(e);
            }
        }
    }
    let dev = dev.ok_or_else(|| {
        anyhow::anyhow!(
            "opening SDR device {args}: {}",
            last_err
                .map(|e| e.to_string())
                .unwrap_or_else(|| "unknown error".into())
        )
    })?;
    let caps = inspect(&dev);
    let _ = event_tx.try_send(Event::Capabilities(caps.clone()));
    let _ = log_tx.try_send(format!(
        "{}: driver={} hardware={} gain={:?}",
        cfg.serial, caps.driver, caps.hardware, caps.gain
    ));

    // A span carried over from another radio may be one this driver does not
    // have — an RSP's 6 MS/s on an RTL, an RTL's 1.536 MS/s on an RSP. Take
    // the nearest it does, rather than failing the open and leaving the
    // server waiting for a radio that is sitting right there.
    let rate = nearest_rate(&caps, cfg.rate);
    dev.set_sample_rate(Rx, 0, rate)
        .context("setting sample rate")?;
    let actual_rate = dev.sample_rate(Rx, 0).unwrap_or(rate);
    if (actual_rate - cfg.rate).abs() >= 1.0 {
        let _ = log_tx.try_send(format!(
            "{}: requested {:.0} S/s, driver selected {:.0} S/s",
            cfg.serial, cfg.rate, actual_rate
        ));
    }
    if let Err(e) = set_bandwidth(&dev, actual_rate, cfg.cover_hz, &log_tx) {
        let _ = log_tx.try_send(format!("{}: analog bandwidth unavailable: {e}", cfg.serial));
    }

    let tune = Tuning {
        desired: cfg.freq,
        ppm: cfg.ppm,
        if_bw: None,
    };
    dev.set_frequency(Rx, 0, corrected(tune.desired, tune.ppm), ())
        .context("setting frequency")?;

    let mut initial_agc = false;
    // The gain mode the driver opened in is not ours to inherit: SoapySDRPlay3
    // opens with the RSP's IF AGC running, and it would quietly override
    // every IF gain reduction written below.
    if caps.hardware_agc && dev.gain_mode(Rx, 0).unwrap_or(false) {
        let _ = set_gain_mode_checked(&dev, false);
    }
    match cfg.gain {
        Some(g) => {
            apply_overall_gain(&dev, &caps, cfg.freq, &mut initial_agc, g, &log_tx);
        }
        None => {
            // "Automatic" gain is a clip-guarded loop on manual gain, never
            // the tuner's own AGC. The R820T's AGC drives the LNA and mixer
            // to full gain regardless of what is on the antenna; measured
            // here it left the ADC at 0.997 of full scale with the noise
            // floor 30 dB up, and every signal but the strongest gone — the
            // receiver "went deaf" until a restart re-applied the saved
            // manual gain. The guard walks gain down within two seconds of
            // clipping and back up, a decibel at a time, when the band is
            // quiet.
            if caps.hardware_agc {
                let _ = set_gain_mode_checked(&dev, false);
            }
            let start = auto_start_for(&caps, cfg.freq);
            apply_overall_gain(&dev, &caps, cfg.freq, &mut initial_agc, start, &log_tx);
            initial_agc = true;
            let _ = log_tx.try_send(format!(
                "{}: auto gain (clip-guarded) starting at {start:.1} dB",
                cfg.serial
            ));
        }
    }
    // The driver's own DC correction, where it has one (SDRplay does: it
    // removes the zero-IF offset before the samples leave the radio). The
    // software blocker in FrontEnd still runs after it; this just leaves it
    // less to do. An RTL module offers no such mode.
    if dev.has_dc_offset_mode(Rx, 0).unwrap_or(false) {
        match dev.set_dc_offset_mode(Rx, 0, true) {
            Ok(()) => { let _ = log_tx.try_send(format!("{}: hardware DC correction on", cfg.serial)); }
            Err(e) => { let _ = log_tx.try_send(format!("{}: hardware DC correction failed: {e}", cfg.serial)); }
        }
    }
    let _ = dev.write_setting("biasT_ctrl", if cfg.bias_t { "true" } else { "false" });
    if caps.bias_t && cfg.bias_t {
        let _ = log_tx.try_send(format!("{}: bias-T on", cfg.serial));
    }
    for (have, want, key) in [
        (caps.rf_notch, cfg.rf_notch, "rfnotch_ctrl"),
        (caps.dab_notch, cfg.dab_notch, "dabnotch_ctrl"),
    ] {
        if let (true, Some(on)) = (have, want) {
            let _ = dev.write_setting(key, if on { "true" } else { "false" });
        }
    }

    publish_state(&dev, &caps, tune, initial_agc, false, &event_tx);

    let serial = cfg.serial.clone();
    let cover_hz = cfg.cover_hz;
    let remote = cfg.remote.is_some();
    let fan = fanout.clone();
    let log_for_worker = log_tx.clone();
    let worker_caps = caps.clone();
    let worker = std::thread::spawn(move || {
        if let Err(e) = run(
            dev,
            worker_caps,
            actual_rate,
            cover_hz,
            remote,
            tune,
            initial_agc,
            cmd_rx,
            fan,
            log_for_worker.clone(),
            event_tx,
        ) {
            let _ = log_for_worker.try_send(format!("{serial}: radio thread stopped: {e:#}"));
        }
    });

    Ok(Device {
        serial: cfg.serial.clone(),
        cmd: cmd_tx,
        log: log_rx,
        events: event_rx,
        iq: fanout,
        rate: actual_rate,
        role: cfg.role.clone(),
        caps,
        worker: Some(worker),
    })
}

fn inspect(dev: &soapysdr::Device) -> Capabilities {
    let driver = dev.driver_key().unwrap_or_else(|_| "unknown".into());
    let hardware = dev.hardware_key().unwrap_or_else(|_| "unknown".into());
    let gains = dev.list_gains(Rx, 0).unwrap_or_default();
    let find = |want: &str| gains.iter().find(|g| g.eq_ignore_ascii_case(want));
    let gain = if let Some((rf, ifg)) = find("RFGR").zip(find("IFGR")) {
        let rr = dev.gain_element_range(Rx, 0, rf.as_str()).ok();
        let ir = dev.gain_element_range(Rx, 0, ifg.as_str()).ok();
        GainControl::Sdrplay {
            rfgr_min: rr.as_ref().map_or(0.0, |r| r.minimum),
            rfgr_max: rr.as_ref().map_or(9.0, |r| r.maximum),
            ifgr_min: ir.as_ref().map_or(20.0, |r| r.minimum),
            ifgr_max: ir.as_ref().map_or(59.0, |r| r.maximum),
        }
    } else {
        let r = dev.gain_range(Rx, 0).ok();
        GainControl::Overall {
            min: r.as_ref().map_or(0.0, |x| x.minimum),
            max: r.as_ref().map_or(49.6, |x| x.maximum),
        }
    };
    let sdrplay = matches!(gain, GainControl::Sdrplay { .. })
        || driver.to_ascii_lowercase().contains("sdrplay");
    let has_setting = |key: &str| sdrplay && dev.read_setting(key).is_ok();
    Capabilities {
        driver,
        hardware,
        gain,
        hardware_agc: dev.has_gain_mode(Rx, 0).unwrap_or(false),
        agc_setpoint: has_setting("agc_setpoint"),
        rf_notch: has_setting("rfnotch_ctrl"),
        dab_notch: has_setting("dabnotch_ctrl"),
        iq_correction: has_setting("iqcorr_ctrl"),
        ppm: dev
            .list_frequencies(Rx, 0)
            .is_ok_and(|v| v.iter().any(|x| x.eq_ignore_ascii_case("CORR"))),
        bias_t: has_setting("biasT_ctrl"),
        if_bandwidths: {
            let ranges = dev.bandwidth_range(Rx, 0).unwrap_or_default();
            let discrete = ranges.iter().all(|r| r.maximum <= r.minimum + r.minimum.abs().max(1.0) * 1e-6);
            let mut v: Vec<f64> = if discrete {
                ranges.iter().map(|r| r.maximum).filter(|v| *v > 0.0).collect()
            } else {
                Vec::new()
            };
            v.sort_by(f64::total_cmp);
            v.dedup();
            v
        },
        rate_ranges: dev
            .get_sample_rate_range(Rx, 0)
            .unwrap_or_default()
            .iter()
            .map(|r| (r.minimum, r.maximum))
            .collect(),
        freq_range: dev.frequency_range(Rx, 0).ok().and_then(|v| {
            let lo = v.iter().map(|r| r.minimum).reduce(f64::min)?;
            let hi = v.iter().map(|r| r.maximum).reduce(f64::max)?;
            Some((lo, hi))
        }),
    }
}

/// What the caller asked for, and the error being compensated for.
///
/// These are kept apart from the number handed to the driver: every consumer
/// reasons about the frequency it asked for, and only the moment of tuning cares
/// that the crystal is wrong.
#[derive(Clone, Copy, Debug)]
struct Tuning {
    /// Frequency last *accepted* by the tuner. Only advanced on success, so a
    /// failed tune leaves it pointing at where the radio really is — which is
    /// both what the rest of the system should be told, and what makes asking
    /// for the same frequency again a real retry rather than a no-op.
    desired: f64,
    ppm: f64,
    /// Analog IF filter asked for by hand; `None` while it follows the span.
    if_bw: Option<f64>,
}

/// The frequency to request so the signal at `desired` lands on centre.
///
/// Correction is done here rather than through the driver's `CORR` control on
/// purpose. librtlsdr's frequency correction takes an **integer** ppm, which is
/// an 800 Hz step at 800 MHz — 6% of a 12.5 kHz P25 channel, and the dongles on
/// this host measure between 1 and 3 ppm, so integer rounding would throw away
/// most of what was just measured. Doing it in the tune request costs nothing,
/// is exact, and behaves identically on every driver.
fn corrected(desired: f64, ppm: f64) -> f64 {
    desired / (1.0 + ppm * 1e-6)
}

/// Past this, a retune is reported in the log: normally 15-30 ms, and a
/// stream that is not being drained loses samples in 65 ms at 8 MS/s.
const SLOW_TUNE: std::time::Duration = std::time::Duration::from_millis(150);

/// How many deliveries the control side may fall behind by before the reader
/// stops telling it about them: five seconds' worth.
const REPORT_DEPTH: usize = 256;

/// What the reader tells the control side about each pass over the stream.
enum ReadReport {
    /// A delivery, with what the gain loops and the health readout need of it.
    Block { samples: usize, peak: f32, clipped: u64, unheard: bool, lagged: u32 },
    /// Samples were lost before they reached the reader.
    Overflow,
    Failed(soapysdr::Error),
}

/// Where each delivery falls on the acquisition timeline, and whether it goes
/// out at all.
///
/// A block is held back while the tuner moves, and so is the one in hand when
/// the hold lifts, which began while the oscillator was still on its way. Held
/// blocks still count as samples, so a tune that was refused, with no new run
/// to explain the hole, shows up downstream as the gap it is.
#[derive(Default)]
struct Timeline {
    sample_pos: u64,
    epoch: u64,
    was_held: bool,
}

impl Timeline {
    /// `(first_sample, epoch)` for a block of `n` samples, or `None` to let
    /// it go.
    fn next(&mut self, n: usize, held: bool, epoch: u64) -> Option<(u64, u64)> {
        if epoch != self.epoch {
            self.epoch = epoch;
            self.sample_pos = 0;
        }
        let deliver = !held && !self.was_held;
        self.was_held = held;
        let first = self.sample_pos;
        self.sample_pos += n as u64;
        deliver.then_some((first, epoch))
    }
}

/// What the reader and the control side share.
#[derive(Default)]
struct ReaderShared {
    /// The stream and its delivery size; empty across a rate change.
    stream: Mutex<Option<(soapysdr::RxStream<Complex32>, usize)>>,
    /// Keeps the reader off `stream`, so the control side gets the lock.
    yielded: AtomicBool,
    /// Bumped on every new run of samples: an accepted tune or rate change,
    /// or an overflow.
    epoch: AtomicU64,
    /// Set across a write that moves the tuner.
    held: AtomicBool,
    stop: AtomicBool,
}

/// The thread that drains the stream and hands the samples out.
///
/// It does nothing else, because everything else can take a long time. A
/// write to an SDRplay waits for the API to confirm it, normally 15-30 ms but
/// measured at 500 ms on a receiver whose LNA was hunting under the IF AGC.
/// The driver's ring is 65 ms deep at 8 MS/s, so one thread doing both lost
/// samples at every retune, and at each of the two re-asserts after it: three
/// half-second holes in the waterfall and the audio. Here a write only holds
/// up the control side.
struct SampleReader {
    shared: Arc<ReaderShared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SampleReader {
    fn start(
        stream: soapysdr::RxStream<Complex32>,
        block_len: usize,
        fanout: Fanout,
        reports: SyncSender<ReadReport>,
    ) -> Result<Self> {
        let shared = Arc::new(ReaderShared::default());
        *shared.stream.lock().expect("stream") = Some((stream, block_len));
        let inner = shared.clone();
        let thread = std::thread::Builder::new()
            .name("sdr-reader".into())
            .spawn(move || read_samples(&inner, &fanout, &reports))
            .context("starting the sample reader")?;
        Ok(Self { shared, thread: Some(thread) })
    }

    /// Stop delivering: the tuner is about to move.
    fn hold(&self) {
        self.shared.held.store(true, Ordering::Release);
    }

    /// Deliver again.
    fn release(&self) {
        self.shared.held.store(false, Ordering::Release);
    }

    /// What follows is a different run of samples.
    fn new_run(&self) {
        self.shared.epoch.fetch_add(1, Ordering::AcqRel);
    }

    /// Take the stream away from the reader and close it.
    fn close(&self) {
        self.shared.yielded.store(true, Ordering::Release);
        if let Some((mut stream, _)) = self.shared.stream.lock().expect("stream").take() {
            let _ = stream.deactivate(None);
        }
    }

    /// Give the reader a new, active stream.
    fn open(&self, stream: soapysdr::RxStream<Complex32>, block_len: usize) {
        *self.shared.stream.lock().expect("stream") = Some((stream, block_len));
        self.shared.yielded.store(false, Ordering::Release);
    }
}

impl Drop for SampleReader {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.close();
    }
}

fn read_samples(shared: &ReaderShared, fanout: &Fanout, reports: &SyncSender<ReadReport>) {
    let mut buf = vec![Complex32::new(0.0, 0.0); MAX_BLOCK];
    let mut timeline = Timeline::default();
    // The control side may be half a second into a driver call; a report it
    // has no room for is a statistic lost, not a sample.
    let report = |r: ReadReport| match reports.try_send(r) {
        Ok(()) | Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
    };
    while !shared.stop.load(Ordering::Acquire) {
        if shared.yielded.load(Ordering::Acquire) {
            std::thread::sleep(std::time::Duration::from_millis(1));
            continue;
        }
        // Gather one delivery's worth (see BLOCK_SECONDS). An error part-way
        // still delivers what arrived before it, then is dealt with below.
        let mut n = 0usize;
        let mut read_err = None;
        {
            let mut slot = shared.stream.lock().expect("stream");
            let Some((stream, block_len)) = slot.as_mut() else {
                drop(slot);
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            };
            let block_len = *block_len;
            while n < block_len {
                match stream.read(&mut [&mut buf[n..block_len]], 1_000_000) {
                    Ok(0) => break,
                    Ok(got) => n += got,
                    Err(e) => {
                        read_err = Some(e);
                        break;
                    }
                }
            }
        }
        if n > 0 {
            let (peak, clipped) = peak_and_clips(&buf[..n]);
            // In this order: a hold seen lifted has its new run in `epoch`.
            let held = shared.held.load(Ordering::Acquire);
            let epoch = shared.epoch.load(Ordering::Acquire);
            let (mut unheard, mut lagged) = (false, 0);
            if let Some((first_sample, epoch)) = timeline.next(n, held, epoch) {
                let block = IqBlock { samples: Arc::from(&buf[..n]), first_sample, epoch };
                if fanout.subscriber_count() == 0 {
                    unheard = true;
                } else {
                    lagged = fanout.broadcast(block);
                }
            }
            report(ReadReport::Block { samples: n, peak, clipped, unheard, lagged });
        }
        match read_err {
            None if n == 0 => std::thread::sleep(std::time::Duration::from_millis(1)),
            None => {}
            // Samples were lost before they reached this thread — the
            // driver's ring, or remotely the server's — and the device is
            // fine, so this is not a strike toward reopening it. A new epoch
            // makes every consumer restart framing rather than splice across
            // the hole, and it goes on the stream health readout.
            Some(e) if e.code == soapysdr::ErrorCode::Overflow => {
                shared.epoch.fetch_add(1, Ordering::AcqRel);
                report(ReadReport::Overflow);
            }
            Some(e) => {
                report(ReadReport::Failed(e));
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    dev: soapysdr::Device,
    caps: Capabilities,
    mut rate: f64,
    cover_hz: f64,
    remote: bool,
    mut tune: Tuning,
    mut agc_enabled: bool,
    cmd_rx: Receiver<Cmd>,
    fanout: Fanout,
    log_tx: SyncSender<String>,
    event_tx: SyncSender<Event>,
) -> Result<()> {
    let (mut stream, block_len) = rx_stream(&dev, &caps.driver, rate, remote, Some(&log_tx))?;
    stream.activate(None)?;
    let (reports_tx, reports) = sync_channel::<ReadReport>(REPORT_DEPTH);
    let reader = SampleReader::start(stream, block_len, fanout, reports_tx)?;
    let mut dropped: u64 = 0;
    let mut lagged: u64 = 0;
    let mut clipped: u64 = 0;
    let mut counted: u64 = 0;
    // Samples since the LNA's take-over last looked.
    let mut quarter: u64 = 0;
    let mut peak: f64 = 0.0;
    // `agc_enabled` is the auto-gain loop. Its guard climbs to the hardware
    // maximum; the operator's optional clip guard (Cmd::ClipGuard) sits
    // under a hand-set gain and never exceeds it. Exactly one of the two,
    // or neither, is live in `clip_guard`.
    let mut guard_wanted = false;
    let mut tuner_agc = false;
    let mut clip_guard = if agc_enabled {
        let (min, max) = gain_range(&caps, tune.desired);
        read_gain(&dev, &caps, tune.desired).map(|g| ClipGuard::auto(g, min, max, gain_steps(&caps)))
    } else {
        None
    };
    // Last gain written, by hand or by a guard. An SDRplay LNA state means a
    // different reduction in each band, so a tune across a band edge writes
    // this again to keep the gain the operator chose.
    let mut gain_target = read_gain(&dev, &caps, tune.desired);
    let mut usb_fails = 0u32;
    let mut tune_fails = 0u32;
    // An SDRplay tune is written again shortly after it lands; see
    // SDRPLAY_REASSERT. The instant the next write is due, and which one.
    let sdrplay = matches!(caps.gain, GainControl::Sdrplay { .. });
    let mut reassert: Option<(std::time::Instant, usize)> = None;
    // Under the RSP's IF AGC: the LNA's own stepping, and the gain last
    // reported, so a readout is sent only when it moves.
    let mut lna_takeover = LnaTakeover::default();
    let mut last_readout = GainReadout::default();

    loop {
        let mut last_tune = None;
        loop {
            let changed = match cmd_rx.try_recv() {
                Ok(Cmd::Quit) => return Ok(()),
                // Several hops can queue while a previous set_frequency is
                // still blocking on a sick R820T. Apply only the latest.
                Ok(Cmd::Tune(f)) => {
                    last_tune = Some(f);
                    false
                }
                Ok(Cmd::Gain(g)) => {
                    let mut leave_hw_agc = agc_enabled || tuner_agc;
                    let ok = apply_overall_gain(&dev, &caps, tune.desired, &mut leave_hw_agc, g, &log_tx);
                    if ok {
                        agc_enabled = false;
                        let (min, max) = gain_range(&caps, tune.desired);
                        let applied = g.clamp(min, max);
                        gain_target = Some(applied);
                        clip_guard = guard_wanted
                            .then(|| ClipGuard::new(applied, min, max, gain_steps(&caps)));
                    }
                    ok
                }
                Ok(Cmd::Rfgr(g)) => {
                    set_and_log(&log_tx, "RFGR", dev.set_gain_element(Rx, 0, "RFGR", g))
                }
                Ok(Cmd::Ifgr(g)) => {
                    set_and_log(&log_tx, "IFGR", dev.set_gain_element(Rx, 0, "IFGR", g))
                }
                Ok(Cmd::TunerAgc(on)) => {
                    // The R820T's own AGC is the only way to the VGA stage:
                    // librtlsdr fixes the VGA at 16.3 dB under manual gain
                    // and at 26.5 dB under its AGC, with LNA and mixer on
                    // the tuner's detector. At full manual gain a quiet UHF
                    // band drove the ADC to 0.06 of full scale — four
                    // effective bits — and weak pages that the tuner AGC had
                    // shown were gone. It overloads on a strong band, which
                    // the ADC CLIPPING warning shows; the operator chooses.
                    tuner_agc = on;
                    if !agc_enabled {
                        false
                    } else if on {
                        let ok = caps.hardware_agc && set_gain_mode_checked(&dev, true);
                        if ok {
                            clip_guard = None;
                            let _ = log_tx.try_send(hardware_agc_note(&caps).into());
                        } else {
                            let _ = log_tx.try_send("tuner AGC unavailable on this device".into());
                        }
                        ok
                    } else {
                        // Back to the guarded loop from its start gain.
                        let (min, max) = gain_range(&caps, tune.desired);
                        let start = auto_start_for(&caps, tune.desired);
                        let mut hw = caps.hardware_agc;
                        let ok = apply_overall_gain(&dev, &caps, tune.desired, &mut hw, start, &log_tx);
                        if ok {
                            gain_target = Some(start);
                            clip_guard = Some(ClipGuard::auto(start, min, max, gain_steps(&caps)));
                            let _ = log_tx.try_send(format!(
                                "auto gain (clip-guarded) starting at {start:.1} dB"
                            ));
                        }
                        ok
                    }
                }
                Ok(Cmd::ClipGuard(on)) => {
                    guard_wanted = on;
                    // Auto gain keeps its own guard whatever the checkbox says.
                    if !agc_enabled {
                        clip_guard = if on {
                            let (min, max) = gain_range(&caps, tune.desired);
                            read_gain(&dev, &caps, tune.desired)
                                .map(|g| ClipGuard::new(g, min, max, gain_steps(&caps)))
                        } else {
                            None
                        };
                    }
                    false
                }
                Ok(Cmd::Agc(on)) => {
                    // See open(): automatic gain is the guarded loop on manual
                    // gain. The tuner's own AGC is never switched on.
                    if on && tuner_agc && caps.hardware_agc {
                        let ok = set_gain_mode_checked(&dev, true);
                        if ok {
                            agc_enabled = true;
                            clip_guard = None;
                            let _ = log_tx.try_send(hardware_agc_note(&caps).into());
                        }
                        ok
                    } else if on {
                        let (min, max) = gain_range(&caps, tune.desired);
                        let start = auto_start_for(&caps, tune.desired);
                        let mut hw_agc = agc_enabled && caps.hardware_agc;
                        let ok = apply_overall_gain(&dev, &caps, tune.desired, &mut hw_agc, start, &log_tx);
                        if ok {
                            agc_enabled = true;
                            gain_target = Some(start);
                            clip_guard = Some(ClipGuard::auto(start, min, max, gain_steps(&caps)));
                            let _ = log_tx.try_send(format!(
                                "auto gain (clip-guarded) starting at {start:.1} dB"
                            ));
                        }
                        ok
                    } else {
                        // Leave the gain where the loop had it; the caller
                        // follows with an explicit Gain to set its own.
                        agc_enabled = false;
                        clip_guard = None;
                        true
                    }
                }
                Ok(Cmd::BiasT(on)) => set_and_log(
                    &log_tx,
                    "bias-T",
                    dev.write_setting("biasT_ctrl", if on { "true" } else { "false" }),
                ),
                Ok(Cmd::RfNotch(on)) => set_and_log(
                    &log_tx,
                    "RF notch",
                    dev.write_setting("rfnotch_ctrl", if on { "true" } else { "false" }),
                ),
                Ok(Cmd::DabNotch(on)) => set_and_log(
                    &log_tx,
                    "DAB notch",
                    dev.write_setting("dabnotch_ctrl", if on { "true" } else { "false" }),
                ),
                Ok(Cmd::AgcSetpoint(db)) => set_and_log(
                    &log_tx,
                    "IF AGC setpoint",
                    dev.write_setting("agc_setpoint", format!("{:.0}", db.clamp(-60.0, 0.0)).as_str()),
                ),
                Ok(Cmd::IqCorrection(on)) => set_and_log(
                    &log_tx,
                    "I/Q correction",
                    dev.write_setting("iqcorr_ctrl", if on { "true" } else { "false" }),
                ),
                Ok(Cmd::IfBandwidth(want)) => {
                    // A filter wider than the sample rate lets everything
                    // outside the span alias into it: never honoured.
                    tune.if_bw = want.filter(|w| *w <= rate * 1.001);
                    match tune.if_bw {
                        Some(w) => {
                            let ok = set_and_log(&log_tx, "IF filter", dev.set_bandwidth(Rx, 0, w));
                            let actual = dev.bandwidth(Rx, 0).unwrap_or(w);
                            let _ = log_tx.try_send(format!("analog bandwidth {actual:.0} Hz (manual)"));
                            ok
                        }
                        None => set_bandwidth(&dev, rate, cover_hz, &log_tx).is_ok(),
                    }
                }
                Ok(Cmd::Ppm(ppm)) => {
                    tune.ppm = ppm;
                    reader.hold();
                    let ok = set_frequency_retrying(&dev, corrected(tune.desired, tune.ppm));
                    if !ok {
                        let _ = log_tx.try_send(
                            "frequency correction failed: tuner would not accept it".into(),
                        );
                    } else {
                        // An accepted correction moves the LO: what follows is
                        // a different sample run even though the dial did not
                        // move. Stale queued blocks must not frame with it.
                        reader.new_run();
                        if sdrplay {
                            reassert = Some((std::time::Instant::now() + SDRPLAY_REASSERT[0], 0));
                        }
                    }
                    reader.release();
                    ok
                }
                Ok(Cmd::Rate(r)) => {
                    reader.close();
                    std::thread::sleep(std::time::Duration::from_millis(25));
                    let ok = dev.set_sample_rate(Rx, 0, r);
                    if let Err(ref e) = ok {
                        let _ = log_tx.try_send(format!("rate failed: {e}"));
                    } else {
                        rate = dev.sample_rate(Rx, 0).unwrap_or(r);
                        // New rate = new sample grid: the old positions mean
                        // nothing against it, so this starts a new epoch.
                        reader.new_run();
                        let _ = log_tx.try_send(format!("sample rate {rate:.0} S/s"));
                        std::thread::sleep(std::time::Duration::from_millis(15));
                        // A hand-picked filter survives a span change only
                        // while it still fits inside the new span.
                        tune.if_bw = tune.if_bw.filter(|w| *w <= rate * 1.001);
                        if let Some(w) = tune.if_bw {
                            let _ = dev.set_bandwidth(Rx, 0, w);
                        } else if let Err(e) = set_bandwidth(&dev, rate, cover_hz, &log_tx) {
                            let _ = log_tx.try_send(format!(
                                "analog bandwidth after rate change unavailable: {e}"
                            ));
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    let (mut stream, block_len) = rx_stream(&dev, &caps.driver, rate, remote, Some(&log_tx))?;
                    stream.activate(None)?;
                    reader.open(stream, block_len);
                    ok.is_ok()
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return Ok(()),
            };
            if changed {
                publish_state(&dev, &caps, tune, agc_enabled, agc_enabled && tuner_agc && clip_guard.is_none(), &event_tx);
            }
        }
        if let Some(f) = last_tune {
            // Skip a no-op hop: librtlsdr still walks the R820T I2C map.
            if (tune.desired - f).abs() >= 1.0 {
                // An I2C write to the R820T fails often enough on a tired
                // dongle that one attempt is not a fair test. Retrying costs
                // a few milliseconds and usually lands, which is the
                // difference between a click that tunes and a click that is
                // silently dropped.
                let started = std::time::Instant::now();
                reader.hold();
                // An LNA state the new band does not have makes SDRplay's
                // API refuse the whole tune (state 8 is legal at 100 MHz, not
                // at 7 MHz), so it comes down to the band's deepest first.
                if sdrplay {
                    fit_lna_to_band(&dev, &caps, f, &log_tx);
                }
                let write = std::time::Instant::now();
                let mut ok = set_frequency_retrying(&dev, corrected(f, tune.ppm));
                let wrote = write.elapsed();
                if !ok && sdrplay {
                    // A model or band this crate has no state count for:
                    // state 0 exists everywhere. The band change below then
                    // writes the chosen gain again for the new band.
                    let _ = crate::driver_log::checked(|| dev.set_gain_element(Rx, 0, "RFGR", 0.0).is_ok());
                    ok = set_frequency_retrying(&dev, corrected(f, tune.ppm));
                    if ok {
                        let _ = log_tx.try_send(format!("tune to {:.4} MHz took only at LNA state 0", f / 1e6));
                    }
                }
                if ok {
                    tune_fails = 0;
                    let before = tune.desired;
                    // Only now is this where the radio is.
                    tune.desired = f;
                    if sdrplay {
                        reassert = Some((std::time::Instant::now() + SDRPLAY_REASSERT[0], 0));
                    }
                    // The same LNA state is a different reduction in the new
                    // band: write the chosen gain again for it, or the dial
                    // reads one gain while the receiver runs another. Not
                    // under the RSP's own IF AGC, which owns the IF stage.
                    let hardware_agc_running = agc_enabled && tuner_agc && clip_guard.is_none();
                    if sdrplay_band_changed(&caps, before, f)
                        && hardware_agc_running
                        && let (Some(sg), Some(g)) = (sdrplay_gain(&caps, f), gain_target)
                        && let (Some(state), _) = sg.split(g)
                    {
                        // The IF stays the AGC's; the LNA state goes back to
                        // what the chosen gain makes in this band, since the
                        // tune may have had to lower it (fit_lna_to_band).
                        let _ = crate::driver_log::checked(|| dev.set_gain_element(Rx, 0, "RFGR", state as f64).is_ok());
                    }
                    if sdrplay_band_changed(&caps, before, f) && !hardware_agc_running {
                        let (min, max) = gain_range(&caps, f);
                        if let Some(cg) = clip_guard.as_mut() {
                            cg.min = min;
                            cg.max = max;
                            cg.ceiling = if agc_enabled { max } else { cg.ceiling.min(max) };
                            cg.current = cg.current.clamp(min, max);
                            gain_target = Some(cg.current);
                        }
                        if let Some(g) = gain_target.map(|g| g.clamp(min, max)) {
                            if set_gain_checked(&dev, &caps, f, g) {
                                gain_target = Some(g);
                            } else {
                                let _ = log_tx.try_send(format!(
                                    "gain {g:.1} dB failed: not taken after the band change"
                                ));
                            }
                        }
                    }
                    publish_state(&dev, &caps, tune, agc_enabled, agc_enabled && tuner_agc && clip_guard.is_none(), &event_tx);
                    // Accepted tune: samples before and after belong to
                    // different RF windows, so a new epoch. After the state
                    // above, so the new run is framed at the new frequency.
                    reader.new_run();
                } else {
                    // The driver may have stored the refused frequency all
                    // the same, and its readback is what the display labels
                    // by: write the last accepted one again so the two agree.
                    if sdrplay {
                        reassert = Some((std::time::Instant::now() + SDRPLAY_REASSERT[0], 0));
                    }
                    tune_fails = tune_fails.saturating_add(1);
                    let _ = log_tx.try_send(format!(
                        "tune failed: {:.4} MHz not accepted after {TUNE_ATTEMPTS} attempts ({tune_fails}/3)",
                        f / 1e6
                    ));
                    if tune_fails >= 3 {
                        anyhow::bail!("tuner wedged after {tune_fails} failed tune attempts");
                    }
                }
                reader.release();
                if started.elapsed() > SLOW_TUNE {
                    let _ = log_tx.try_send(format!(
                        "tune to {:.4} MHz held the tuner {:.0} ms ({:.0} ms of it the frequency write)",
                        f / 1e6,
                        started.elapsed().as_secs_f64() * 1e3,
                        wrote.as_secs_f64() * 1e3
                    ));
                }
            }
        }

        if let Some((due, stage)) = reassert
            && std::time::Instant::now() >= due
        {
            // No new epoch: when the earlier write did land this moves the LO
            // by one hertz, and a decoder that has just locked should keep
            // its lock. When it did not, the samples before were already off
            // frequency and nothing downstream was told otherwise.
            if !set_frequency_retrying(&dev, reassert_hz(corrected(tune.desired, tune.ppm), stage)) {
                let _ = log_tx.try_send(format!(
                    "tune re-assert at {:.4} MHz not accepted",
                    tune.desired / 1e6
                ));
            }
            reassert = SDRPLAY_REASSERT
                .get(stage + 1)
                .map(|gap| (std::time::Instant::now() + *gap, stage + 1));
        }

        // What the reader did since the last look; the commands above wait
        // at most this long.
        let report = match reports.recv_timeout(std::time::Duration::from_millis(25)) {
            Ok(report) => report,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => anyhow::bail!("the sample reader stopped"),
        };
        match report {
            ReadReport::Overflow => {
                dropped += 1;
                let _ = log_tx.try_send("stream overflow: samples lost before delivery".into());
            }
            ReadReport::Failed(e) => {
                usb_fails += 1;
                let _ = log_tx.try_send(format!("read error ({usb_fails}/3): {e}"));
                if usb_fails >= 3 {
                    anyhow::bail!("dongle not responding after {usb_fails} read errors: {e}");
                }
            }
            ReadReport::Block { samples: n, peak: block_peak, clipped: block_clipped, unheard, lagged: late } => {
                usb_fails = 0;
                clipped += block_clipped;
                peak = peak.max(f64::from(block_peak));
                counted += n as u64;
                dropped += u64::from(unheard);
                lagged += u64::from(late);

                // The LNA's take-over, four times a second: from a cold start
                // with the LNA wide open the IF is pinned at once, and there is
                // nothing to wait for.
                quarter += n as u64;
                if sdrplay && quarter >= (rate / 4.0) as u64 {
                    quarter = 0;
                    let if_agc = agc_enabled && tuner_agc && clip_guard.is_none();
                    let live = sdrplay_live(&dev);
                    let rfgr = dev.gain_element(Rx, 0, "RFGR").ok();
                    // The LNA follows the IF AGC, so the front end is not
                    // left at full gain while the IF runs out of range.
                    if if_agc
                        && let (Some(sg), Some(ifgr), Some(state)) = (sdrplay_gain(&caps, tune.desired), live.ifgr, rfgr)
                        && !sg.lna_gr.is_empty()
                    {
                        let top = sg.lna_gr.len().min(sdrplay_lna_states(&caps, tune.desired).unwrap_or(sg.lna_gr.len())) - 1;
                        if let Some(next) = lna_takeover.on_tick(sg.lna_gr, ifgr, live.overload == Some(true), state.round().max(0.0) as usize, top)
                            && crate::driver_log::checked(|| dev.set_gain_element(Rx, 0, "RFGR", next as f64).is_ok())
                        {
                            let _ = log_tx.try_send(format!(
                                "LNA state {next} ({:.0} dB reduction): the IF AGC was at {ifgr:.0} dB{}",
                                sg.lna_gr[next],
                                if live.overload == Some(true) { ", overloaded" } else { "" }
                            ));
                        }
                    } else {
                        lna_takeover = LnaTakeover::default();
                    }
                }

                if counted >= rate as u64 {
                    let clipped_fraction = clipped as f64 / counted.max(1) as f64;
                    if let Some(ref mut cg) = clip_guard
                        && let Some(new_g) = cg.on_stats(clipped_fraction, peak)
                    {
                        // Same retried write as a hand-set gain: on this
                        // tuner the first attempt routinely stalls and the
                        // second lands. A single attempt here, logged as a
                        // failure, tripped the server's fault counter and
                        // reopened the device every sixteen seconds.
                        if set_gain_checked(&dev, &caps, tune.desired, new_g) {
                            gain_target = Some(new_g);
                            let _ = log_tx.try_send(format!(
                                "gain {new_g:.1} dB (clip {clipped_fraction:.4} peak {peak:.2})"
                            ));
                            publish_state(&dev, &caps, tune, agc_enabled, false, &event_tx);
                        } else {
                            // Only after every retry: counted by the server
                            // as a control fault, two of which in a window
                            // reopen the device — what clears a stalled bus.
                            let _ = log_tx.try_send(format!(
                                "gain {new_g:.1} dB failed: tuner did not take the write"
                            ));
                            cg.current = cg.current.min(new_g);
                        }
                    }
                    if sdrplay {
                        let if_agc = agc_enabled && tuner_agc && clip_guard.is_none();
                        let live = sdrplay_live(&dev);
                        let rfgr = dev.gain_element(Rx, 0, "RFGR").ok();
                        let ifgr = if if_agc { live.ifgr } else { dev.gain_element(Rx, 0, "IFGR").ok() };
                        let readout = GainReadout {
                            overall: sdrplay_overall(&caps, tune.desired, rfgr, ifgr),
                            rfgr,
                            ifgr,
                            overload: live.overload,
                            overload_count: live.overload_count,
                        };
                        if readout != last_readout {
                            let _ = event_tx.try_send(Event::Gain(readout.clone()));
                            last_readout = readout;
                        }
                    }
                    peak = 0.0;
                    let _ = event_tx.try_send(Event::StreamStats {
                        dropped_blocks: dropped,
                        lagged_deliveries: lagged,
                        clipped_fraction,
                    });
                    clipped = 0;
                    counted = 0;
                }
            }
        }
    }
}

/// Manual overall gain, clamped to what this tuner can actually do.
///
/// RTL-SDR reports a wide Soapy range (96 dB on the E4000) but librtlsdr
/// saturates earlier (52 dB on that chip, 49.6 dB on the R820T). Asking
/// above the real ceiling is treated as "maximum". An SDRplay's gain is the
/// combined LNA and IF scale of [`SdrplayGain`], at the band `freq_hz` is in.
fn apply_overall_gain(
    dev: &soapysdr::Device,
    caps: &Capabilities,
    freq_hz: f64,
    agc_enabled: &mut bool,
    want: f64,
    log: &SyncSender<String>,
) -> bool {
    if caps.hardware_agc && *agc_enabled {
        if set_gain_mode_checked(dev, false) {
            *agc_enabled = false;
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    let (min, max) = gain_range(caps, freq_hz);
    let g = snap_gain(gain_steps(caps), want.clamp(min, max));
    let ok = set_gain_checked(dev, caps, freq_hz, g);
    if ok {
        let got = read_gain(dev, caps, freq_hz).unwrap_or(g);
        if (want - got).abs() > 0.15 {
            let _ = log.try_send(format!("gain {want:.1} dB → {got:.1} dB"));
        } else {
            let _ = log.try_send(format!("gain {got:.1} dB"));
        }
        true
    } else {
        let _ = log.try_send(format!("gain {g} failed"));
        false
    }
}

/// Where the auto-gain loop starts: well below the maximum, so a strong
/// band is not clipped for the seconds the guard takes to walk down (2 dB
/// per two seconds), and high enough that a quiet band is usable while it
/// climbs. On an R820T with a modest antenna the ADC clipped from about
/// 28 dB up, so 40% of the range (~20 dB) starts under that.
fn auto_gain_start(min: f64, max: f64) -> f64 {
    (min + (max - min) * 0.4).clamp(min, max)
}

/// Where the auto-gain loop starts on this device.
///
/// Not 40% on an SDRplay: on its scale that lands on a deep LNA state, and
/// SDRplay's measured noise figure there is 40–50 dB — a receiver that
/// starts deaf and spends half a minute climbing out. It starts instead at
/// the LNA take-over point (full LNA gain, IF reduced to the take-over
/// level), which leaves the 14-bit ADC far more headroom than an R820T's
/// 8 bits had when the 40% rule was set.
fn auto_start_for(caps: &Capabilities, freq_hz: f64) -> f64 {
    match sdrplay_gain(caps, freq_hz) {
        Some(sg) => sg.gain_of(sg.lna_gr.first().map(|_| 0), SDRPLAY_IF_TAKEOVER_DB)
            .unwrap_or(0.0),
        None => {
            let (min, max) = gain_range(caps, freq_hz);
            auto_gain_start(min, max)
        }
    }
}

/// `want` if the driver takes it, else the closest rate it does.
fn nearest_rate(caps: &Capabilities, want: f64) -> f64 {
    caps.rate_ranges
        .iter()
        .map(|&(lo, hi)| want.clamp(lo, hi))
        .min_by(|a, b| (a - want).abs().total_cmp(&(b - want).abs()))
        .unwrap_or(want)
}

/// The gain range `Cmd::Gain` is clamped to, at `freq_hz`.
fn gain_range(caps: &Capabilities, freq_hz: f64) -> (f64, f64) {
    match (caps.gain.clone(), sdrplay_gain(caps, freq_hz)) {
        (_, Some(sg)) => (0.0, sg.max_gain()),
        (GainControl::Overall { min, max }, None) => (min, max),
        (GainControl::Sdrplay { .. }, None) => (0.0, 0.0),
    }
}

/// The gain the hardware is at, on the same scale.
fn read_gain(dev: &soapysdr::Device, caps: &Capabilities, freq_hz: f64) -> Option<f64> {
    let Some(sg) = sdrplay_gain(caps, freq_hz) else {
        return dev.gain(Rx, 0).ok();
    };
    let ifgr = dev.gain_element(Rx, 0, "IFGR").ok()?;
    let state = if sg.lna_gr.is_empty() {
        None
    } else {
        Some(dev.gain_element(Rx, 0, "RFGR").ok()?.round().max(0.0) as usize)
    };
    sg.gain_of(state, ifgr)
}

fn hardware_agc_note(caps: &Capabilities) -> &'static str {
    if matches!(caps.gain, GainControl::Sdrplay { .. }) {
        "IF AGC on: IF gain under the RSP's own loop, the LNA following it"
    } else {
        "tuner AGC on: LNA, mixer and VGA under the tuner's own control"
    }
}

/// IF gain reduction the [`SdrplayGain`] split holds while the LNA takes
/// over. The first 20 dB of reduction from full gain comes off the IF stage,
/// which costs no noise figure; past that the LNA steps in, because a signal
/// strong enough to need more is strong enough to be driving the LNA toward
/// overload (SDRplay's IIP3 rises about 6 dB per LNA state), and then IF
/// takes the rest once the LNA has nothing left. It is the classic delayed
/// RF AGC take-over, and IFGR 40 is also where SDRplay's own IF AGC tends to
/// sit at its -30 dBFS setpoint.
const SDRPLAY_IF_TAKEOVER_DB: f64 = 40.0;

/// How an SDRplay receiver's two reduction controls make one gain in dB, so
/// the single RF GAIN control and the clip-guarded loop work on it the way
/// they do on an RTL-SDR.
///
/// The driver has two *reduction* controls: IFGR, 20–59 dB and exact, and
/// RFGR, an LNA state index whose reduction depends on the band.
/// SoapySDRPlay3's own overall gain sums the raw numbers — decibels plus an
/// index — and calls larger *less* gain, so requests on this crate's
/// higher-is-more scale would run backwards through it.
///
/// Gain here is decibels above the deepest reduction used: 0 is everything
/// reduced, the maximum is LNA state 0 with IFGR at its floor. Like
/// librtlsdr's R820T table the scale is nominal; see [`rsp1a_lna_gr_db`] for
/// where its numbers come from. For a model without a table only the IF
/// stage moves, which is exact, and the LNA state is left alone.
#[derive(Clone, Copy, Debug, PartialEq)]
struct SdrplayGain {
    /// LNA reduction per state, dB, ascending; empty when not known.
    lna_gr: &'static [f64],
    ifgr_min: f64,
    ifgr_max: f64,
}

impl SdrplayGain {
    fn lna_max(&self) -> f64 {
        self.lna_gr.last().copied().unwrap_or(0.0)
    }

    fn max_gain(&self) -> f64 {
        (self.ifgr_max - self.ifgr_min) + self.lna_max()
    }

    /// LNA state (`None`: leave it alone) and IFGR that realize `gain_db`.
    fn split(&self, gain_db: f64) -> (Option<usize>, f64) {
        let total = self.ifgr_max + self.lna_max() - gain_db.clamp(0.0, self.max_gain());
        if self.lna_gr.is_empty() {
            return (None, total.round().clamp(self.ifgr_min, self.ifgr_max));
        }
        // The least LNA reduction that keeps IF at or below the take-over
        // level; past the last state, IF takes everything left. A state
        // whose step would push IF under its floor is skipped for the one
        // before it, which realizes the same total exactly.
        let mut state = self.lna_gr.len() - 1;
        for (s, &gr) in self.lna_gr.iter().enumerate() {
            if total - gr <= SDRPLAY_IF_TAKEOVER_DB {
                state = if total - gr < self.ifgr_min && s > 0 { s - 1 } else { s };
                break;
            }
        }
        let ifgr = (total - self.lna_gr[state]).round().clamp(self.ifgr_min, self.ifgr_max);
        (Some(state), ifgr)
    }

    /// The gain an LNA state and IFGR make; `None` for a state the table
    /// does not cover, whose reduction is not known.
    fn gain_of(&self, state: Option<usize>, ifgr: f64) -> Option<f64> {
        let lna = match state {
            None => 0.0,
            Some(s) => *self.lna_gr.get(s)?,
        };
        Some(self.ifgr_max + self.lna_max() - (ifgr + lna))
    }
}

/// What the patched SoapySDRPlay3 passes on from SDRplay's API events (see
/// `tools/soapysdrplay3-live-gain.patch`): the IF gain reduction and LNA
/// reduction the API last reported, and its overload state. Each is `None`
/// from a driver without the patch, which answers unknown keys with "".
#[derive(Clone, Copy, Debug, Default)]
struct SdrplayLive {
    ifgr: Option<f64>,
    overload: Option<bool>,
    overload_count: Option<u64>,
}

/// The combined gain an LNA state and IF reduction make at `freq_hz`;
/// `None` when either is not known.
fn sdrplay_overall(caps: &Capabilities, freq_hz: f64, rfgr: Option<f64>, ifgr: Option<f64>) -> Option<f64> {
    let sg = sdrplay_gain(caps, freq_hz)?;
    let state = if sg.lna_gr.is_empty() { None } else { Some(rfgr?.round().max(0.0) as usize) };
    sg.gain_of(state, ifgr?)
}

fn sdrplay_live(dev: &soapysdr::Device) -> SdrplayLive {
    let read = |k: &str| dev.read_setting(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    SdrplayLive {
        ifgr: read("live_ifgr").and_then(|v| v.parse().ok()),
        overload: read("overload").and_then(|v| match v.as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        }),
        overload_count: read("overload_count").and_then(|v| v.parse().ok()),
    }
}

/// IF gain reduction past which the LNA takes more, and under which it
/// gives some back, while the RSP's IF AGC runs: the take-over level ±10 dB,
/// so a 6 dB LNA step (19 dB at the top of 60–420 MHz) lands inside the
/// window rather than across it.
const LNA_UP_IFGR: f64 = SDRPLAY_IF_TAKEOVER_DB + 10.0;
const LNA_DOWN_IFGR: f64 = SDRPLAY_IF_TAKEOVER_DB - 10.0;
/// Quarter-second ticks the IF must sit under [`LNA_DOWN_IFGR`] before the
/// LNA gives gain back (three seconds): slow, so a fade does not walk the
/// front end back into overload.
const LNA_DOWN_HOLD: u32 = 12;
/// Ticks of rest after the LNA takes gain, for the IF AGC to settle on it.
const LNA_REST: u32 = 2;
/// Ticks of rest after the LNA gives gain back: two seconds, and not to be cut
/// short. The IF AGC reads high for a second or more after a step up in gain,
/// 53-59 dB where it settles at about 39. Taken at face value that sent the
/// LNA straight back, and at the 19 dB step from state 8 to state 7 on the
/// broadcast FM band the receiver swung between the two every few seconds.
const LNA_SETTLE: u32 = 8;
/// A single LNA step of this much or more, the 19 dB ones in the RSP1A's
/// tables, is not taken on one reading.
const LNA_DEEP_STEP_DB: f64 = 15.0;
/// Ticks the IF must stay over [`LNA_UP_IFGR`] before such a step (two
/// seconds), unless the API reports overload. For a moment after a big tune
/// or an LNA step the IF AGC reads 59 dB whatever the signal is. Believed, that
/// took the broadcast FM band from state 7 (IF at 39 dB) to state 8, which
/// leaves the IF on its 20 dB floor and the AGC with nowhere to go.
const LNA_DEEP_HOLD: u32 = 8;

/// The LNA under the RSP's IF AGC. The AGC moves only the IF stage, so on
/// a band with a strong local station it ran out of reduction (59 dB) with
/// the LNA at full gain, and the overdriven front end painted that station
/// across the band as intermodulation. Four times a second this steps the
/// LNA so the IF stays near the take-over level: deeper at once by as much
/// as the IF is over it (or a state on an overload report), back a state at
/// a time once the IF has been well under it for a while. A step is
/// followed by half a second's rest while the AGC settles.
#[derive(Clone, Copy, Debug, Default)]
struct LnaTakeover {
    cold: u32,
    hot: u32,
    rest: u32,
}

impl LnaTakeover {
    /// The state to move to, given the band's reductions, the IF gain
    /// reduction the AGC is at, whether the API reports overload, the state
    /// now and the deepest the band allows.
    fn on_tick(&mut self, lna_gr: &[f64], ifgr: f64, overload: bool, state: usize, top: usize) -> Option<usize> {
        let state = state.min(top);
        if self.rest > 0 {
            self.rest -= 1;
            return None;
        }
        if (ifgr >= LNA_UP_IFGR || overload) && state < top {
            self.cold = 0;
            // The AGC gives back in IF what the LNA takes, so this lands
            // the IF near the take-over level in one step.
            let want = (ifgr - SDRPLAY_IF_TAKEOVER_DB).max(1.0);
            let mut next = (state + 1..=top).find(|&s| lna_gr[s] - lna_gr[state] >= want).unwrap_or(top);
            if !overload && let Some(short) = (state..next).find(|&s| lna_gr[s + 1] - lna_gr[s] >= LNA_DEEP_STEP_DB) {
                if short > state {
                    next = short;
                } else {
                    self.hot += 1;
                    if self.hot < LNA_DEEP_HOLD {
                        return None;
                    }
                }
            }
            self.hot = 0;
            self.rest = LNA_REST;
            return Some(next);
        }
        self.hot = 0;
        if ifgr <= LNA_DOWN_IFGR && state > 0 {
            self.cold += 1;
            if self.cold >= LNA_DOWN_HOLD {
                self.cold = 0;
                self.rest = LNA_SETTLE;
                return Some(state - 1);
            }
            return None;
        }
        self.cold = 0;
        None
    }
}

/// LNA gain reduction for each RSP1A LNA state, in dB, for the band `freq_hz`
/// is in.
///
/// SDRplay's nominal per-band values, cross-checked against the gain SDRplay
/// measured for every LNA state across frequency in its RSP1A technical
/// information. The two agree to within about 3 dB for every state listed.
/// Where they disagree about what a state does — the 420–1000 MHz band
/// beyond state 3 (the measurement has a different number of states there),
/// the top state at 60–420 MHz (measured at 296 MHz giving *more* gain than
/// the state below it) and the top two above 1 GHz (measured in the other
/// order) — the state is left out rather than guessed at. That costs depth
/// of reduction in those bands, never accuracy; IF makes up the rest.
fn rsp1a_lna_gr_db(freq_hz: f64) -> &'static [f64] {
    const BELOW_60_MHZ: &[f64] = &[0.0, 6.0, 12.0, 18.0, 37.0, 42.0, 61.0];
    const BELOW_420_MHZ: &[f64] = &[0.0, 6.0, 12.0, 18.0, 20.0, 26.0, 32.0, 38.0, 57.0];
    const BELOW_1000_MHZ: &[f64] = &[0.0, 7.0, 13.0, 19.0];
    const ABOVE_1000_MHZ: &[f64] = &[0.0, 6.0, 12.0, 20.0, 26.0, 32.0, 38.0];
    if freq_hz < 60e6 {
        BELOW_60_MHZ
    } else if freq_hz < 420e6 {
        BELOW_420_MHZ
    } else if freq_hz < 1000e6 {
        BELOW_1000_MHZ
    } else {
        ABOVE_1000_MHZ
    }
}

/// The SDRplay gain model at `freq_hz`, or `None` for an RTL-SDR.
fn sdrplay_gain(caps: &Capabilities, freq_hz: f64) -> Option<SdrplayGain> {
    let GainControl::Sdrplay { ifgr_min, ifgr_max, .. } = caps.gain else {
        return None;
    };
    let lna_gr = if caps.hardware.eq_ignore_ascii_case("RSP1A") {
        rsp1a_lna_gr_db(freq_hz)
    } else {
        &[]
    };
    Some(SdrplayGain { lna_gr, ifgr_min, ifgr_max })
}

/// How many LNA states SDRplay's API allows at `freq_hz`, for the models
/// whose counts are known (`sdrplay_api_rsp1a.h`): fewer below 60 MHz and
/// above 1 GHz than in between.
fn sdrplay_lna_states(caps: &Capabilities, freq_hz: f64) -> Option<usize> {
    if !caps.hardware.eq_ignore_ascii_case("RSP1A") {
        return None;
    }
    Some(if freq_hz < 60e6 {
        7
    } else if freq_hz < 1000e6 {
        10
    } else {
        9
    })
}

/// Bring the LNA state within what the band at `freq_hz` has, before tuning
/// there. Nothing is written when it already fits.
fn fit_lna_to_band(dev: &soapysdr::Device, caps: &Capabilities, freq_hz: f64, log_tx: &SyncSender<String>) {
    let Some(states) = sdrplay_lna_states(caps, freq_hz) else { return };
    let Ok(now) = dev.gain_element(Rx, 0, "RFGR") else { return };
    let top = (states - 1) as f64;
    if now.round() > top && !crate::driver_log::checked(|| dev.set_gain_element(Rx, 0, "RFGR", top).is_ok()) {
        let _ = log_tx.try_send(format!("LNA state {top} for {:.4} MHz not taken", freq_hz / 1e6));
    }
}

/// Whether a tune from `a` to `b` crossed an SDRplay LNA band edge.
fn sdrplay_band_changed(caps: &Capabilities, a: f64, b: f64) -> bool {
    sdrplay_gain(caps, a) != sdrplay_gain(caps, b)
}

/// Set the tuner gain, retrying a write the driver reported as taken but
/// the stderr watcher saw fail. The driver reports success whether or not
/// the I2C write took; the watcher is what says it did.
/// Switch the tuner's own AGC on or off, retried like every other tuner
/// write: the mode change is an I2C write to the same registers and stalls
/// the same way, and the driver reports it as taken regardless.
fn set_gain_mode_checked(dev: &soapysdr::Device, automatic: bool) -> bool {
    for attempt in 0..GAIN_ATTEMPTS {
        if crate::driver_log::checked(|| dev.set_gain_mode(Rx, 0, automatic).is_ok()) {
            return true;
        }
        if attempt + 1 < GAIN_ATTEMPTS {
            std::thread::sleep(GAIN_RETRY_DELAY * (attempt + 1));
        }
    }
    false
}

fn set_gain_checked(dev: &soapysdr::Device, caps: &Capabilities, freq_hz: f64, g: f64) -> bool {
    let write = || match sdrplay_gain(caps, freq_hz) {
        None => dev.set_gain(Rx, 0, g).is_ok(),
        Some(sg) => {
            let (state, ifgr) = sg.split(g);
            state.is_none_or(|s| dev.set_gain_element(Rx, 0, "RFGR", s as f64).is_ok())
                && dev.set_gain_element(Rx, 0, "IFGR", ifgr).is_ok()
        }
    };
    for attempt in 0..GAIN_ATTEMPTS {
        if crate::driver_log::checked(write) {
            return true;
        }
        if attempt + 1 < GAIN_ATTEMPTS {
            std::thread::sleep(GAIN_RETRY_DELAY * (attempt + 1));
        }
    }
    false
}

/// Attempts per tuner write, and the pause between them. The pause grows
/// per attempt: a stalled control endpoint that fails at 6 ms sometimes
/// clears by 30, and a write that still fails after that is reported so
/// the receiver can be reopened, which is what does clear it.
const TUNE_ATTEMPTS: u32 = 4;
const TUNE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(8);
const GAIN_ATTEMPTS: u32 = 4;

/// After an SDRplay tune lands, write it again this long after, then again
/// this much later.
///
/// SoapySDRPlay3 stores the new frequency before asking the API to apply it,
/// and when the API refuses (an earlier update, often a gain write from the
/// clip guard, still in flight) or times out, it logs a warning on the
/// machine the radio is on and returns as if it had worked. Its readback is
/// the stored value, and asking again for the same frequency is skipped as
/// unchanged, so the hardware stays where it was — 50 kHz off after one
/// dropped hop on a 2026-11-02 RSP1A, which a calibration then read as
/// -236 ppm. Over SoapyRemote the warning never reaches this process, so
/// there is nothing to check; writing again is what makes the tune stick.
const SDRPLAY_REASSERT: [std::time::Duration; 2] =
    [std::time::Duration::from_millis(300), std::time::Duration::from_millis(1_200)];

/// The frequency a re-assert writes: one hertz above the last write, then
/// back on it, so the driver sees a change each time and really updates the
/// tuner. A hertz is 0.01 ppm at 100 MHz.
fn reassert_hz(corrected_hz: f64, stage: usize) -> f64 {
    if stage % 2 == 0 { corrected_hz + 1.0 } else { corrected_hz }
}
const GAIN_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(8);

/// Set the tuner frequency, retrying a stalled I2C write.
///
/// Every path that moves the oscillator goes through here. The ppm correction
/// used to write once and give up, so on a dongle that stalls occasionally a
/// calibration would silently not take — the setting was stored, the radio
/// never moved, and nothing said so.
fn set_frequency_retrying(dev: &soapysdr::Device, hz: f64) -> bool {
    for attempt in 0..TUNE_ATTEMPTS {
        // A partial write (the mux registers taken, the PLL not) leaves the
        // tuner deaf at both frequencies; only a clean full write counts.
        // Each retry is a hertz further on: SoapySDRPlay3 has already stored
        // the refused frequency and skips a request for the same one as
        // unchanged, so a plain repeat would "succeed" without a write. A
        // few hertz is a few hundredths of a ppm at VHF.
        let hz = hz + f64::from(attempt);
        if crate::driver_log::checked(|| dev.set_frequency(Rx, 0, hz, ()).is_ok()) {
            return true;
        }
        if attempt + 1 < TUNE_ATTEMPTS {
            std::thread::sleep(TUNE_RETRY_DELAY * (attempt + 1));
        }
    }
    false
}

fn set_and_log(
    log: &SyncSender<String>,
    what: &str,
    result: std::result::Result<(), soapysdr::Error>,
) -> bool {
    match result {
        Ok(()) => true,
        Err(e) => {
            let _ = log.try_send(format!("{what} failed: {e}"));
            false
        }
    }
}

/// The requested frequency is reported rather than the driver's readback: the
/// hardware is deliberately tuned off by the correction, and a consumer that saw
/// that number would think it had been mistuned.
fn publish_state(
    dev: &soapysdr::Device,
    caps: &Capabilities,
    tune: Tuning,
    auto_gain: bool,
    if_agc: bool,
    tx: &SyncSender<Event>,
) {
    // Report the frequency the tuner *reached*, not the one it was asked for.
    // An R820T synthesises its LO from integer dividers and cannot land on
    // every requested value; reporting the request means the display insists on
    // a frequency the hardware is not on, and the error grows with frequency
    // because the divider step does. Converted back out of the ppm-corrected
    // domain so it is comparable with what the operator asked for.
    let achieved = dev
        .frequency(Rx, 0)
        .ok()
        .map(|lo| lo * (1.0 + tune.ppm * 1e-6));
    let mut state = State {
        // The guarded loop, not the tuner's gain mode (always manual now).
        agc: Some(auto_gain),
        rate: dev.sample_rate(Rx, 0).ok(),
        bandwidth: dev.bandwidth(Rx, 0).ok(),
        frequency: achieved.or(Some(tune.desired)),
        ppm: Some(tune.ppm),
        overall_gain: read_gain(dev, caps, tune.desired),
        gain_range: Some(gain_range(caps, tune.desired)),
        bandwidth_manual: tune.if_bw.is_some(),
        ..State::default()
    };
    if let GainControl::Sdrplay { .. } = caps.gain {
        state.rfgr = dev.gain_element(Rx, 0, "RFGR").ok();
        let live = sdrplay_live(dev);
        // Under the IF AGC the driver's IFGR is the last value set by hand,
        // not where the AGC has it: only the live report will do, and
        // without one the IF is not known.
        state.ifgr = if if_agc { live.ifgr } else { dev.gain_element(Rx, 0, "IFGR").ok() };
        state.overload = live.overload;
        state.overload_count = live.overload_count;
        state.lna_steps_db = sdrplay_gain(caps, tune.desired).map(|sg| sg.lna_gr.to_vec()).unwrap_or_default();
        state.lna_auto = if_agc && !state.lna_steps_db.is_empty();
        // From the same readings, so the dial does not show a gain the IF
        // AGC has long since moved away from.
        state.overall_gain = sdrplay_overall(caps, tune.desired, state.rfgr, state.ifgr);
    }
    let switch = |have: bool, key: &str| {
        have.then(|| dev.read_setting(key).ok())
            .flatten()
            .map(|v| v.eq_ignore_ascii_case("true"))
    };
    state.bias_t = switch(caps.bias_t, "biasT_ctrl");
    state.rf_notch = switch(caps.rf_notch, "rfnotch_ctrl");
    state.dab_notch = switch(caps.dab_notch, "dabnotch_ctrl");
    state.iq_correction = switch(caps.iq_correction, "iqcorr_ctrl");
    state.agc_setpoint = caps
        .agc_setpoint
        .then(|| dev.read_setting("agc_setpoint").ok())
        .flatten()
        .and_then(|v| v.trim().parse::<f64>().ok());
    let _ = tx.try_send(Event::State(state));
}

/// The narrowest offered filter that covers `cover_hz` without exceeding
/// `rate`; failing that the widest that fits under `rate`, since alias
/// rejection is worth more than the last decibel at the span edge.
///
/// Kept from hfscan, where the lesson was learned the hard way: asking for a
/// filter as wide as the sample rate makes the driver round *down* to one of its
/// handful of discrete widths, putting the corner inside the digitised span.
fn choose_bandwidth(options: &[f64], rate: f64, cover_hz: f64) -> f64 {
    let fits = |w: &&f64| **w <= rate * 1.001;
    options
        .iter()
        .filter(fits)
        .find(|w| **w >= cover_hz)
        .or_else(|| options.iter().filter(fits).next_back())
        .copied()
        .unwrap_or(rate)
}

fn set_bandwidth(
    dev: &soapysdr::Device,
    rate: f64,
    cover_hz: f64,
    log_tx: &SyncSender<String>,
) -> Result<()> {
    // A driver reporting one continuous range must not be read as "two choices,
    // the endpoints" — picking an endpoint there is worse than the rounding this
    // is meant to fix, so a continuous range is honoured by asking for what is
    // actually wanted.
    let ranges = dev.bandwidth_range(Rx, 0).unwrap_or_default();
    let continuous = ranges
        .iter()
        .find(|r| r.maximum > r.minimum + r.minimum.abs().max(1.0) * 1e-6);
    let want = if let Some(r) = continuous {
        cover_hz.max(r.minimum).min(rate.min(r.maximum))
    } else {
        let mut options: Vec<f64> = ranges
            .iter()
            .map(|r| r.maximum)
            .filter(|v| *v > 0.0)
            .collect();
        options.sort_by(f64::total_cmp);
        options.dedup();
        // "No requirement" is the whole span here. Asking a continuous range
        // for zero gets the driver's auto width; asking a list for the
        // narrowest filter that covers zero got an RSP's 200 kHz filter in
        // front of a 2 MHz span.
        let cover = if cover_hz > 0.0 { cover_hz } else { rate };
        choose_bandwidth(&options, rate, cover)
    };
    dev.set_bandwidth(Rx, 0, want)?;
    let actual = dev.bandwidth(Rx, 0).unwrap_or(want);
    let _ = log_tx.try_send(format!("analog bandwidth {actual:.0} Hz"));
    Ok(())
}

#[cfg(test)]
mod reader_tests {
    use super::Timeline;

    #[test]
    fn samples_are_counted_on_one_run() {
        let mut t = Timeline::default();
        assert_eq!(t.next(100, false, 0), Some((0, 0)));
        assert_eq!(t.next(100, false, 0), Some((100, 0)));
    }

    #[test]
    fn nothing_goes_out_while_the_tuner_moves_nor_the_block_it_ends_inside() {
        let mut t = Timeline::default();
        assert_eq!(t.next(100, false, 0), Some((0, 0)));
        assert_eq!(t.next(100, true, 0), None);
        assert_eq!(t.next(100, true, 0), None);
        // The hold lifted during this block: its start may be mid-move.
        assert_eq!(t.next(100, false, 1), None);
        // The run after a tune starts at the first block after the hold.
        assert_eq!(t.next(100, false, 1), Some((100, 1)));
        assert_eq!(t.next(100, false, 1), Some((200, 1)));
    }

    #[test]
    fn a_refused_tune_leaves_the_hole_visible() {
        let mut t = Timeline::default();
        assert_eq!(t.next(100, false, 0), Some((0, 0)));
        assert_eq!(t.next(100, true, 0), None);
        assert_eq!(t.next(100, false, 0), None);
        // The same run with its position moved on: downstream sees the gap.
        assert_eq!(t.next(100, false, 0), Some((300, 0)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deliveries_divide_a_display_frame() {
        // 20 ms wherever it fits.
        assert_eq!(block_for(2e6), 40_000);
        assert_eq!(block_for(6e6), 120_000);
        assert_eq!(block_for(250e3), 5_000);
        // Past the cap, a whole fraction of 40 ms in 4096-sample blocks.
        for (rate, parts) in [(8e6, 3.0), (10e6, 4.0)] {
            let b = block_for(rate);
            assert!(b <= MAX_BLOCK && b % 4096 == 0, "{b}");
            let ms = b as f64 * parts / rate * 1e3;
            assert!((ms - 40.0).abs() < 1.0, "{rate}: {parts} deliveries are {ms} ms");
        }
    }

    #[test]
    fn the_peak_scan_matches_one_sample_at_a_time() {
        let mut seed = 0x9e37_79b9u32;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as f32 / u32::MAX as f32 * 2.2 - 1.1
        };
        for n in [0usize, 1, 7, 8, 9, 1000, 4099] {
            let iq: Vec<Complex32> = (0..n).map(|_| Complex32::new(rnd(), rnd())).collect();
            let (mut peak, mut clips) = (0.0f32, 0u64);
            for c in &iq {
                let m = c.re.abs().max(c.im.abs());
                peak = peak.max(m);
                clips += u64::from(m > 0.98);
            }
            assert_eq!(peak_and_clips(&iq), (peak, clips), "{n} samples");
        }
    }

    /// SoapySDRPlay3 skips a write whose `(uint32_t)` Hz matches what it
    /// stored; every re-assert must differ from the write before it, or it
    /// is the same no-op that left the RSP1A 50 kHz off.
    #[test]
    fn every_sdrplay_reassert_is_a_change_the_driver_acts_on() {
        for want in [104_552_917.6, 162_550_000.0, 1_000_000.4] {
            let stored = |hz: f64| hz as u32;
            let mut last = stored(want);
            for stage in 0..SDRPLAY_REASSERT.len() {
                let next = stored(reassert_hz(want, stage));
                assert_ne!(next, last, "stage {stage} at {want} would be skipped");
                assert!((reassert_hz(want, stage) - want).abs() <= 1.0);
                last = next;
            }
        }
    }

    /// Widths an R820T reports through the rtlsdr Soapy module.
    const R820T: [f64; 6] = [
        300_000.0,
        600_000.0,
        1_000_000.0,
        1_536_000.0,
        2_000_000.0,
        2_400_000.0,
    ];

    #[test]
    fn the_filter_covers_the_channel_rather_than_matching_the_span() {
        // A 12.5 kHz P25 channel in a 2.4 MS/s span: take the narrowest filter
        // on offer, not the widest that fits, or alias rejection is thrown away.
        assert_eq!(choose_bandwidth(&R820T, 2_400_000.0, 12_500.0), 300_000.0);
        // A 1.5 MHz slice of a trunked site needs the filter that covers it.
        assert_eq!(
            choose_bandwidth(&R820T, 2_400_000.0, 1_500_000.0),
            1_536_000.0
        );
    }

    #[test]
    fn a_filter_never_exceeds_what_is_digitised() {
        for (rate, cover) in [(2_400_000.0, 2_400_000.0), (1_024_000.0, 1_500_000.0)] {
            let got = choose_bandwidth(&R820T, rate, cover);
            assert!(
                got <= rate * 1.001,
                "chose {got:.0} Hz for a {rate:.0} Hz span"
            );
        }
    }

    /// Widths SoapySDRPlay3 lists for an RSP1A.
    const RSP1A_BW: [f64; 8] = [
        200_000.0, 300_000.0, 600_000.0, 1_536_000.0, 5_000_000.0, 6_000_000.0, 7_000_000.0,
        8_000_000.0,
    ];

    /// With no channel to protect, the span is what must stay flat: the
    /// widest RSP filter that fits under it, never its narrowest.
    #[test]
    fn a_whole_span_on_an_rsp_gets_the_widest_filter_that_fits() {
        assert_eq!(choose_bandwidth(&RSP1A_BW, 2_048_000.0, 2_048_000.0), 1_536_000.0);
        assert_eq!(choose_bandwidth(&RSP1A_BW, 6_000_000.0, 6_000_000.0), 6_000_000.0);
        assert_eq!(choose_bandwidth(&RSP1A_BW, 250_000.0, 250_000.0), 200_000.0);
    }

    #[test]
    fn no_reported_options_falls_back_to_the_span() {
        assert_eq!(choose_bandwidth(&[], 2_400_000.0, 12_500.0), 2_400_000.0);
    }

    #[test]
    fn a_tight_channel_cluster_selects_the_narrowest_analog_filter() {
        // LCD: three channels in 30 kHz, LO parked 90 kHz off mid to clear DC.
        let center = 155_107_500.0 + 90_000.0;
        let cover = cover_hz(
            center,
            [155_107_500.0, 155_092_500.0, 155_122_500.0],
            11_000.0,
        );
        // 2*105 kHz + 11 kHz + 80 kHz guard ≈ 301 kHz → 600 kHz on the R820T,
        // which keeps every LCD channel well inside ±300 kHz.
        assert!(cover > 280_000.0);
        assert_eq!(choose_bandwidth(&R820T, 2_048_000.0, cover), 600_000.0);
    }

    #[test]
    fn an_offset_tuned_p25_channel_sits_inside_the_analog_filter() {
        // 100 kHz offset + 12.5 kHz + 200 kHz guard → 412 kHz → 600 kHz IF.
        // Carrier at 100 kHz is well inside ±300 kHz.
        let cover = cover_hz_offset(100_000.0, 12_500.0);
        assert!(cover > 400_000.0);
        assert_eq!(choose_bandwidth(&R820T, 2_400_000.0, cover), 600_000.0);
        assert!(100_000.0 < cover / 2.0);
    }

    #[test]
    fn the_control_rate_fits_the_analog_filter_the_offset_demands() {
        // 960 kS/s: the 412 kHz cover request is honoured with the 600 kHz
        // IF, fully inside the ±480 kHz Nyquist window — nothing aliases.
        // (At 240 kS/s the same request fell back to the digitised span,
        // leaving 120–150 kHz to fold onto the channel at −100 kHz.)
        let cover = cover_hz_offset(100_000.0, 12_500.0);
        assert_eq!(choose_bandwidth(&R820T, 960_000.0, cover), 600_000.0);
        assert!(cover < 960_000.0);
    }

    #[test]
    fn the_old_250k_offset_needs_a_wider_filter() {
        let cover = cover_hz_offset(250_000.0, 12_500.0);
        assert_eq!(choose_bandwidth(&R820T, 2_400_000.0, cover), 1_000_000.0);
    }

    #[test]
    fn clip_guard_backs_off_then_restores_toward_the_ceiling() {
        let mut g = ClipGuard::new(30.0, 0.0, 49.6, &[]);
        assert!(g.on_stats(0.0, 0.4).is_none());
        assert_eq!(g.on_stats(0.01, 1.0), None, "one hot second is not enough");
        assert_eq!(g.on_stats(0.01, 1.0), Some(28.0));
        // Eight clean seconds creep 1 dB back toward 30.
        for _ in 0..7 {
            assert!(g.on_stats(0.0, 0.4).is_none());
        }
        assert_eq!(g.on_stats(0.0, 0.4), Some(29.0));
        for _ in 0..8 {
            g.on_stats(0.0, 0.4);
        }
        assert_eq!(g.current, 30.0, "must not exceed the configured ceiling");
    }

    /// A hand-set gain is replaced by a fresh guard at that gain (Cmd::Gain
    /// rebuilds it), so the new ceiling is simply the new guard's start.
    #[test]
    fn clip_guard_never_exceeds_a_new_lower_ceiling() {
        let mut g = ClipGuard::new(20.0, 0.0, 49.6, &[]);
        for _ in 0..20 {
            assert!(g.on_stats(0.0, 0.4).is_none());
        }
        assert_eq!(g.current, 20.0);
    }

    /// The auto-gain guard starts below the maximum, climbs to it on a quiet
    /// band, and comes back down as soon as the ADC clips.
    #[test]
    fn auto_gain_guard_climbs_to_max_and_backs_off_on_clipping() {
        let start = auto_gain_start(0.0, 49.6);
        assert!((start - 19.84).abs() < 0.01);
        let mut g = ClipGuard::auto(start, 0.0, 49.6, R82XX_GAINS_DB);
        let mut steps = 0;
        for _ in 0..400 {
            if g.on_stats(0.0, 0.4).is_some() {
                steps += 1;
            }
        }
        assert!(steps > 0 && (g.current - 49.6).abs() < 0.05, "climbed to {}", g.current);
        // With 12 dB of headroom the climb is 3 dB every two seconds: a
        // quiet band goes from the start to full gain in about twenty.
        let mut q = ClipGuard::auto(start, 0.0, 49.6, R82XX_GAINS_DB);
        let mut secs = 0;
        while (q.current - 49.6).abs() > 0.05 && secs < 200 {
            q.on_stats(0.0, 0.05);
            secs += 1;
        }
        assert!(secs <= 22, "quiet band took {secs} s to reach full gain");
        assert!(g.on_stats(0.01, 1.0).is_none());
        // Sitting at 0.65 of full scale is not clipping, but it is no room
        // to climb either: the loop must hold, not hunt.
        let mut h = ClipGuard::auto(8.7, 0.0, 49.6, R82XX_GAINS_DB);
        for _ in 0..40 {
            assert!(h.on_stats(0.0, 0.65).is_none(), "climbed from 8.7 with the peak at 0.65");
        }
        // Down 2 dB from 49.6 is 47.6, which the tuner cannot do: the next
        // table entry at or below it is 44.5.
        assert_eq!(g.on_stats(0.01, 1.0), Some(44.5));
    }

    /// Every gain the loop asks for is one the tuner can actually set.
    #[test]
    fn auto_gain_guard_only_requests_table_gains() {
        let mut g = ClipGuard::auto(auto_gain_start(0.0, 49.6), 0.0, 49.6, R82XX_GAINS_DB);
        assert!(R82XX_GAINS_DB.contains(&g.current), "start {}", g.current);
        for i in 0..300 {
            let clip = if i % 40 > 30 { 0.02 } else { 0.0 };
            if let Some(next) = g.on_stats(clip, 0.1) {
                assert!(R82XX_GAINS_DB.contains(&next), "requested {next}");
            }
        }
    }

    /// A dongle measured at +2.61 ppm puts a 773.9583 MHz signal about 2 kHz
    /// low; the corrected request has to be lower by exactly that much.
    #[test]
    fn correction_moves_the_request_by_the_measured_error() {
        let want = 773_958_300.0;
        let ppm = 2.61;
        let asked = corrected(want, ppm);
        let shift = want - asked;
        assert!(
            (shift - 2020.0).abs() < 5.0,
            "correction shifted {shift:.0} Hz, expected about 2020"
        );
        // Tuning there makes the oscillator land on the signal.
        assert!((asked * (1.0 + ppm * 1e-6) - want).abs() < 0.5);
    }

    #[test]
    fn zero_ppm_is_a_passthrough() {
        assert_eq!(corrected(155_107_500.0, 0.0), 155_107_500.0);
    }

    /// Sub-ppm corrections must survive, which is the whole reason this is not
    /// left to librtlsdr's integer-ppm control.
    #[test]
    fn fractional_ppm_is_not_rounded_away() {
        let a = corrected(773_958_300.0, 2.0);
        let b = corrected(773_958_300.0, 2.61);
        assert!(
            (a - b).abs() > 400.0,
            "0.61 ppm should be ~470 Hz at 774 MHz"
        );
    }

    #[test]
    fn config_addresses_a_device_by_serial() {
        let c = DeviceConfig::new("00000003", 155_107_500.0, 2_400_000.0);
        assert_eq!(c.soapy_args(), "driver=rtlsdr,serial=00000003");
    }

    #[test]
    fn a_local_sdrplay_is_addressed_by_its_own_driver() {
        let mut c = DeviceConfig::new("210304DD98", 100e6, 6e6);
        c.driver = SDRPLAY;
        assert_eq!(c.soapy_args(), "driver=sdrplay,serial=210304DD98");
    }

    #[test]
    fn a_remote_config_addresses_the_sdrplay_on_that_host_over_tcp() {
        let mut c = DeviceConfig::new("210304DD98", 100e6, 2_048_000.0);
        c.remote = Some(remote_url("mario"));
        assert_eq!(
            c.soapy_args(),
            "driver=remote,remote=tcp://mario:55132,remote:driver=sdrplay,remote:prot=tcp,serial=210304DD98"
        );
    }

    #[test]
    fn remote_specs_become_urls_and_urls_give_back_their_host() {
        assert_eq!(remote_url("mario"), "tcp://mario:55132");
        assert_eq!(remote_url(" mario:6000 "), "tcp://mario:6000");
        assert_eq!(remote_url("tcp://10.0.0.2:55132"), "tcp://10.0.0.2:55132");
        assert_eq!(remote_url("[fd7a::1]"), "tcp://[fd7a::1]:55132");
        assert_eq!(remote_url("[fd7a::1]:7000"), "tcp://[fd7a::1]:7000");
        assert_eq!(remote_host("tcp://mario:55132"), "mario");
        assert_eq!(remote_host("tcp://[fd7a::1]:55132"), "[fd7a::1]");
        assert_eq!(remote_host("tcp://[fd7a::1]"), "[fd7a::1]");
    }

    #[test]
    fn the_model_is_read_out_of_an_sdrplay_label() {
        assert_eq!(sdrplay_model("SDRplay Dev0 RSP1A 210304DD98"), "RSP1A");
        assert_eq!(sdrplay_model("something else"), "SDRplay");
    }

    #[test]
    fn the_lna_follows_the_if_agc() {
        // 60–420 MHz: 0, 6, 12, 18, 20, 26, 32, 38, 57 dB.
        let gr = rsp1a_lna_gr_db(100e6);
        let top = gr.len() - 1;
        let mut t = LnaTakeover::default();
        // The IF out of reduction with the LNA wide open: deeper at once,
        // by at least the 19 dB the IF is over the take-over level (state 4).
        assert_eq!(t.on_tick(gr, 59.0, false, 0, top), Some(4));
        // Half a second's rest while the AGC settles, whatever it reads.
        for _ in 0..LNA_REST {
            assert_eq!(t.on_tick(gr, 59.0, false, 4, top), None);
        }
        // Still pinned: deeper again straight after, so a cold start is out
        // of overload within about a second.
        assert_eq!(t.on_tick(gr, 57.0, false, 4, top), Some(7));
        for _ in 0..LNA_REST {
            t.on_tick(gr, 39.0, false, 7, top);
        }
        // Inside the window: left alone.
        assert_eq!(t.on_tick(gr, 45.0, false, 7, top), None);
        assert_eq!(t.on_tick(gr, 35.0, false, 7, top), None);
        // An overload report steps it even inside the window.
        assert_eq!(t.on_tick(gr, 42.0, true, 7, top), Some(8));
        for _ in 0..LNA_REST {
            t.on_tick(gr, 42.0, false, 8, top);
        }
        // Well under the window: a state back, only after three seconds.
        for _ in 0..LNA_DOWN_HOLD - 1 {
            assert_eq!(t.on_tick(gr, 25.0, false, 8, top), None);
        }
        assert_eq!(t.on_tick(gr, 25.0, false, 8, top), Some(7));
        // A reading back in the window restarts that count.
        let mut t = LnaTakeover::default();
        for _ in 0..LNA_DOWN_HOLD - 1 {
            t.on_tick(gr, 25.0, false, 3, top);
        }
        assert_eq!(t.on_tick(gr, 40.0, false, 3, top), None);
        for _ in 0..LNA_DOWN_HOLD - 1 {
            assert_eq!(t.on_tick(gr, 25.0, false, 3, top), None);
        }
        // Never past the deepest state, never under the shallowest.
        // Gain given back: the AGC's overshoot is not a reason to take it
        // again, and once settled the step holds.
        let mut t = LnaTakeover::default();
        for _ in 0..LNA_DOWN_HOLD - 1 {
            assert_eq!(t.on_tick(gr, 22.0, false, 8, top), None);
        }
        assert_eq!(t.on_tick(gr, 22.0, false, 8, top), Some(7));
        for _ in 0..LNA_SETTLE {
            assert_eq!(t.on_tick(gr, 59.0, false, 7, top), None);
        }
        assert_eq!(t.on_tick(gr, 39.0, false, 7, top), None);

        let mut t = LnaTakeover::default();
        assert_eq!(t.on_tick(gr, 59.0, true, top, top), None);
        for _ in 0..2 * LNA_DOWN_HOLD {
            assert_eq!(t.on_tick(gr, 20.0, false, 0, top), None);
        }
    }

    #[test]
    fn the_lna_wants_a_steady_reading_before_a_19_db_step() {
        let gr = rsp1a_lna_gr_db(100e6);
        let top = gr.len() - 1;
        let mut t = LnaTakeover::default();
        // However pinned the IF reads, a walk stops short of the big step.
        assert_eq!(t.on_tick(gr, 59.0, false, 4, top), Some(7));
        for _ in 0..LNA_REST {
            assert_eq!(t.on_tick(gr, 59.0, false, 7, top), None);
        }
        // The stale reading after the step does not take it...
        for _ in 0..LNA_DEEP_HOLD - 1 {
            assert_eq!(t.on_tick(gr, 59.0, false, 7, top), None);
        }
        // ...and one reading back in the window starts the count over.
        assert_eq!(t.on_tick(gr, 39.0, false, 7, top), None);
        for _ in 0..LNA_DEEP_HOLD - 1 {
            assert_eq!(t.on_tick(gr, 55.0, false, 7, top), None);
        }
        // A signal that really is that strong gets there.
        assert_eq!(t.on_tick(gr, 55.0, false, 7, top), Some(8));
        // An overload report does not wait.
        let mut t = LnaTakeover::default();
        assert_eq!(t.on_tick(gr, 45.0, true, 7, top), Some(8));
        // Below 60 MHz the step sits mid-table: the walk stops at state 3.
        let hf = rsp1a_lna_gr_db(7e6);
        let mut t = LnaTakeover::default();
        assert_eq!(t.on_tick(hf, 59.0, false, 0, hf.len() - 1), Some(3));
    }

    #[test]
    fn the_gain_split_never_picks_an_lna_state_the_band_lacks() {
        // SDRplay's API refuses a tune into a band without the LNA state in
        // use, so every state the gain table can choose must exist there.
        let caps = rsp1a();
        for hz in [1e6, 7.241e6, 14.298e6, 59.9e6, 60e6, 101.3e6, 419e6, 420e6, 999e6, 1000e6, 1090e6, 1999e6] {
            let sg = sdrplay_gain(&caps, hz).unwrap();
            let states = sdrplay_lna_states(&caps, hz).unwrap();
            for g in 0..=sg.max_gain() as i32 {
                if let (Some(s), _) = sg.split(g as f64) {
                    assert!(s < states, "{g} dB picks LNA state {s} at {hz} Hz, which has {states}");
                }
            }
        }
        // Where the 60 MHz edge bit: state 8 is legal at FM, not at 40 m.
        assert!(8 < sdrplay_lna_states(&caps, 101.3e6).unwrap());
        assert!(8 >= sdrplay_lna_states(&caps, 7.241e6).unwrap());
    }

    fn rsp1a() -> Capabilities {
        Capabilities {
            driver: "SDRplay".into(),
            hardware: "RSP1A".into(),
            gain: GainControl::Sdrplay {
                rfgr_min: 0.0,
                rfgr_max: 9.0,
                ifgr_min: 20.0,
                ifgr_max: 59.0,
            },
            hardware_agc: true,
            agc_setpoint: true,
            rf_notch: true,
            dab_notch: true,
            iq_correction: true,
            ppm: true,
            bias_t: true,
            if_bandwidths: RSP1A_BW.to_vec(),
            // What SoapySDRPlay3 reports for an RSP1A.
            rate_ranges: vec![(250_000.0, 250_000.0), (1e6, 1e6), (2e6, 10.66e6)],
            freq_range: Some((1e3, 2e9)),
        }
    }

    const BANDS_HZ: [f64; 4] = [10e6, 100e6, 460e6, 1200e6];

    /// Every gain on the scale is realized to within the IF stage's 1 dB
    /// step, by a state the table covers, and the realized gain never goes
    /// down as the request goes up — in every band.
    #[test]
    fn the_sdrplay_gain_scale_round_trips_and_is_monotonic_in_every_band() {
        let caps = rsp1a();
        for f in BANDS_HZ {
            let sg = sdrplay_gain(&caps, f).unwrap();
            let (lo, hi) = gain_range(&caps, f);
            assert_eq!(lo, 0.0);
            assert!((hi - (39.0 + sg.lna_max())).abs() < 1e-9);
            let mut last = f64::NEG_INFINITY;
            let mut want = 0.0;
            while want <= hi {
                let (state, ifgr) = sg.split(want);
                assert!((20.0..=59.0).contains(&ifgr), "IFGR {ifgr} at {want} dB, {f} Hz");
                let got = sg.gain_of(state, ifgr).expect("a state in the table");
                assert!((got - want).abs() <= 0.5, "{want} dB realized as {got} at {f} Hz");
                assert!(got >= last - 1e-9, "gain went down at {want} dB, {f} Hz");
                last = got;
                want += 0.25;
            }
        }
    }

    /// Reduction comes off the IF stage first and the LNA only past the
    /// take-over point; the deepest reductions use the LNA's deepest state.
    #[test]
    fn the_lna_takes_over_only_after_the_first_twenty_db_of_if_reduction() {
        let sg = sdrplay_gain(&rsp1a(), 100e6).unwrap();
        let top = sg.max_gain();
        assert_eq!(sg.split(top), (Some(0), 20.0));
        assert_eq!(sg.split(top - 20.0), (Some(0), 40.0));
        assert_eq!(sg.split(top - 21.0).0, Some(1));
        assert_eq!(sg.split(0.0), (Some(sg.lna_gr.len() - 1), 59.0));
    }

    /// An SDRplay's auto gain starts with the LNA wide open, not on the deep
    /// LNA state 40% of its range would pick.
    #[test]
    fn sdrplay_auto_gain_starts_at_the_take_over_point() {
        let caps = rsp1a();
        for f in BANDS_HZ {
            let sg = sdrplay_gain(&caps, f).unwrap();
            assert_eq!(sg.split(auto_start_for(&caps, f)), (Some(0), 40.0));
        }
    }

    #[test]
    fn a_state_the_table_does_not_cover_reads_as_unknown() {
        let sg = sdrplay_gain(&rsp1a(), 600e6).unwrap();
        assert_eq!(sg.gain_of(Some(9), 40.0), None);
    }

    /// Another SDRplay model moves only its IF stage, which is exact.
    #[test]
    fn an_sdrplay_without_a_table_moves_only_the_if_stage() {
        let mut caps = rsp1a();
        caps.hardware = "RSPdx".into();
        let sg = sdrplay_gain(&caps, 100e6).unwrap();
        assert_eq!(gain_range(&caps, 100e6), (0.0, 39.0));
        assert_eq!(sg.split(10.0), (None, 49.0));
        assert_eq!(sg.gain_of(None, 49.0), Some(10.0));
    }

    #[test]
    fn band_edges_are_where_the_lna_table_changes() {
        let caps = rsp1a();
        assert!(!sdrplay_band_changed(&caps, 100e6, 400e6));
        assert!(sdrplay_band_changed(&caps, 400e6, 440e6));
        let mut rtl = rsp1a();
        rtl.gain = GainControl::Overall { min: 0.0, max: 49.6 };
        assert!(!sdrplay_band_changed(&rtl, 10e6, 1200e6));
    }

    #[test]
    fn rates_are_checked_against_the_driver_ranges() {
        let caps = rsp1a();
        assert!(caps.supports_rate(250_000.0));
        assert!(caps.supports_rate(6e6));
        assert!(!caps.supports_rate(1_536_000.0));
    }

    /// A span from the other radio opens at the nearest one this one has.
    #[test]
    fn an_unoffered_rate_opens_at_the_nearest_offered_one() {
        assert_eq!(nearest_rate(&rsp1a(), 1_536_000.0), 2e6);
        assert_eq!(nearest_rate(&rsp1a(), 2_400_000.0), 2_400_000.0);
        let mut rtl = rsp1a();
        // What SoapyRTLSDR reports.
        rtl.rate_ranges = vec![(225_001.0, 300_000.0), (900_001.0, 3_200_000.0)];
        assert_eq!(nearest_rate(&rtl, 6e6), 3_200_000.0);
        assert_eq!(nearest_rate(&rtl, 500_000.0), 300_000.0);
    }
}
