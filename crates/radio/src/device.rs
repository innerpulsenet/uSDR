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

use crate::fanout::{Fanout, IqBlock};
use anyhow::{Context, Result};
use num_complex::Complex32;
use soapysdr::Direction::Rx;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};

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
}

#[derive(Clone, Debug, Default)]
pub struct State {
    pub overall_gain: Option<f64>,
    pub rfgr: Option<f64>,
    pub ifgr: Option<f64>,
    pub agc: Option<bool>,
    pub ppm: Option<f64>,
    pub rate: Option<f64>,
    pub bandwidth: Option<f64>,
    pub frequency: Option<f64>,
}

#[derive(Clone, Debug)]
pub enum Event {
    Capabilities(Capabilities),
    State(State),
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
    Rfgr(f64),
    Ifgr(f64),
    Agc(bool),
    BiasT(bool),
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
        }
    }

    /// The Soapy device string this config addresses.
    pub fn soapy_args(&self) -> String {
        format!("driver=rtlsdr,serial={}", self.serial)
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
struct ClipGuard {
    ceiling: f64,
    current: f64,
    min: f64,
    max: f64,
    high: u8,
    low: u8,
}

impl ClipGuard {
    fn new(gain: f64, min: f64, max: f64) -> Self {
        let g = gain.clamp(min, max);
        Self {
            ceiling: g,
            current: g,
            min,
            max,
            high: 0,
            low: 0,
        }
    }

    /// Guard for automatic gain: starts at `start` and may climb all the
    /// way to the hardware maximum on a quiet band, walking back down as
    /// soon as the ADC clips.
    fn auto(start: f64, min: f64, max: f64) -> Self {
        let mut g = Self::new(start, min, max);
        g.ceiling = max;
        g
    }

    /// One observation of clip fraction (0..=1), typically once a second.
    /// Returns a new gain when the hardware should be updated.
    fn on_stats(&mut self, clipped_fraction: f64) -> Option<f64> {
        const CLIP_HIGH: f64 = 0.001;
        const CLIP_LOW: f64 = 0.0001;
        const HIGH_NEED: u8 = 2;
        const LOW_NEED: u8 = 8;
        const DOWN_DB: f64 = 2.0;
        const UP_DB: f64 = 1.0;

        if clipped_fraction > CLIP_HIGH {
            self.high = self.high.saturating_add(1);
            self.low = 0;
            if self.high >= HIGH_NEED {
                self.high = 0;
                let next = (self.current - DOWN_DB).max(self.min);
                if (next - self.current).abs() > 0.05 {
                    self.current = next;
                    return Some(self.current);
                }
            }
        } else if clipped_fraction < CLIP_LOW && self.current < self.ceiling - 0.05 {
            self.low = self.low.saturating_add(1);
            self.high = 0;
            if self.low >= LOW_NEED {
                self.low = 0;
                let next = (self.current + UP_DB).min(self.ceiling).min(self.max);
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
}

/// Enumerate attached RTL-SDR dongles.
///
/// Pinned to `driver=rtlsdr` on purpose: the osmosdr Soapy module is also
/// installed on this host and will happily claim the same hardware, which makes
/// enumeration order and device count depend on which module answers first.
pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    let found = soapysdr::enumerate("driver=rtlsdr").context("enumerating rtlsdr devices")?;
    Ok(found
        .iter()
        .map(|a| DeviceInfo {
            serial: a.get("serial").unwrap_or_default().to_string(),
            label: a.get("label").unwrap_or_default().to_string(),
            tuner: a.get("tuner").unwrap_or_default().to_string(),
        })
        .collect())
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

fn rx_stream(
    dev: &soapysdr::Device,
    driver: &str,
    rate: f64,
    log_tx: Option<&SyncSender<String>>,
) -> Result<soapysdr::RxStream<Complex32>> {
    let mut args = soapysdr::Args::new();
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
        if let Some(log) = log_tx {
            let samples = bufflen / 2;
            let ms = (samples as f64 / rate) * 1000.0;
            let _ = log.try_send(format!(
                "rtlsdr stream config: bufflen={bufflen} ({samples} samples, {ms:.1} ms), buffers=48, asyncBuffs=32"
            ));
        }
    }
    dev.rx_stream_args::<Complex32, _>(&[0], args)
        .map_err(|e| anyhow::anyhow!("rx stream: {e}"))
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

    dev.set_sample_rate(Rx, 0, cfg.rate)
        .context("setting sample rate")?;
    let actual_rate = dev.sample_rate(Rx, 0).unwrap_or(cfg.rate);
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
    };
    dev.set_frequency(Rx, 0, corrected(tune.desired, tune.ppm), ())
        .context("setting frequency")?;

    let mut initial_agc = false;
    match cfg.gain {
        Some(g) => {
            apply_overall_gain(&dev, &caps, &mut initial_agc, g, &log_tx);
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
                let _ = dev.set_gain_mode(Rx, 0, false);
            }
            if let GainControl::Overall { min, max } = caps.gain {
                let start = auto_gain_start(min, max);
                apply_overall_gain(&dev, &caps, &mut initial_agc, start, &log_tx);
                initial_agc = true;
                let _ = log_tx.try_send(format!(
                    "{}: auto gain (clip-guarded) starting at {start:.1} dB",
                    cfg.serial
                ));
            }
        }
    }
    let _ = dev.write_setting("biasT_ctrl", "false");

    publish_state(&dev, &caps, tune, initial_agc, &event_tx);

    let serial = cfg.serial.clone();
    let cover_hz = cfg.cover_hz;
    let fan = fanout.clone();
    let log_for_worker = log_tx.clone();
    let worker = std::thread::spawn(move || {
        if let Err(e) = run(
            dev,
            caps,
            actual_rate,
            cover_hz,
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

#[allow(clippy::too_many_arguments)]
fn run(
    dev: soapysdr::Device,
    caps: Capabilities,
    mut rate: f64,
    cover_hz: f64,
    mut tune: Tuning,
    mut agc_enabled: bool,
    cmd_rx: Receiver<Cmd>,
    fanout: Fanout,
    log_tx: SyncSender<String>,
    event_tx: SyncSender<Event>,
) -> Result<()> {
    let mut stream = rx_stream(&dev, &caps.driver, rate, Some(&log_tx))?;
    stream.activate(None)?;
    let mut buf = vec![Complex32::new(0.0, 0.0); MAX_BLOCK];
    let mut dropped: u64 = 0;
    let mut lagged: u64 = 0;
    let mut clipped: u64 = 0;
    let mut counted: u64 = 0;
    // Acquisition timeline: `sample_pos` is where the next read lands on the
    // device's sample grid; `epoch` bumps on every ACCEPTED tune/rate change
    // (and starts a new run when the thread opens the stream). Consumers use
    // the pair to detect lost deliveries and stale queued blocks exactly.
    let mut sample_pos: u64 = 0;
    let mut epoch: u64 = 0;
    // `agc_enabled` is the auto-gain loop. Its guard climbs to the hardware
    // maximum; the operator's optional clip guard (Cmd::ClipGuard) sits
    // under a hand-set gain and never exceeds it. Exactly one of the two,
    // or neither, is live in `clip_guard`.
    let mut guard_wanted = false;
    let mut clip_guard = if agc_enabled {
        match caps.gain {
            GainControl::Overall { min, max } => {
                dev.gain(Rx, 0).ok().map(|g| ClipGuard::auto(g, min, max))
            }
            GainControl::Sdrplay { .. } => None,
        }
    } else {
        None
    };
    let mut usb_fails = 0u32;
    let mut tune_fails = 0u32;

    loop {
        let mut last_tune = None;
        loop {
            let changed = match cmd_rx.try_recv() {
                Ok(Cmd::Quit) => {
                    let _ = stream.deactivate(None);
                    return Ok(());
                }
                // Several hops can queue while a previous set_frequency is
                // still blocking on a sick R820T. Apply only the latest.
                Ok(Cmd::Tune(f)) => {
                    last_tune = Some(f);
                    false
                }
                Ok(Cmd::Gain(g)) => {
                    let ok = apply_overall_gain(&dev, &caps, &mut agc_enabled, g, &log_tx);
                    if ok {
                        agc_enabled = false;
                        let applied = match caps.gain {
                            GainControl::Overall { min, max } => g.clamp(min, max),
                            GainControl::Sdrplay { .. } => g,
                        };
                        clip_guard = match caps.gain {
                            GainControl::Overall { min, max } if guard_wanted => {
                                Some(ClipGuard::new(applied, min, max))
                            }
                            _ => None,
                        };
                    }
                    ok
                }
                Ok(Cmd::Rfgr(g)) => {
                    set_and_log(&log_tx, "RFGR", dev.set_gain_element(Rx, 0, "RFGR", g))
                }
                Ok(Cmd::Ifgr(g)) => {
                    set_and_log(&log_tx, "IFGR", dev.set_gain_element(Rx, 0, "IFGR", g))
                }
                Ok(Cmd::ClipGuard(on)) => {
                    guard_wanted = on;
                    // Auto gain keeps its own guard whatever the checkbox says.
                    if !agc_enabled {
                        clip_guard = if !on {
                            None
                        } else if let GainControl::Overall { min, max } = caps.gain {
                            dev.gain(Rx, 0).ok().map(|g| ClipGuard::new(g, min, max))
                        } else {
                            None
                        };
                    }
                    false
                }
                Ok(Cmd::Agc(on)) => {
                    // See open(): automatic gain is the guarded loop on manual
                    // gain. The tuner's own AGC is never switched on.
                    if on {
                        match caps.gain {
                            GainControl::Overall { min, max } => {
                                let start = auto_gain_start(min, max);
                                let mut hw_agc = agc_enabled && caps.hardware_agc;
                                let ok = apply_overall_gain(&dev, &caps, &mut hw_agc, start, &log_tx);
                                if ok {
                                    agc_enabled = true;
                                    clip_guard = Some(ClipGuard::auto(start, min, max));
                                    let _ = log_tx.try_send(format!(
                                        "auto gain (clip-guarded) starting at {start:.1} dB"
                                    ));
                                }
                                ok
                            }
                            GainControl::Sdrplay { .. } => {
                                let _ = log_tx.try_send("auto gain unavailable on this device".into());
                                false
                            }
                        }
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
                Ok(Cmd::Ppm(ppm)) => {
                    tune.ppm = ppm;
                    let ok = set_frequency_retrying(&dev, corrected(tune.desired, tune.ppm));
                    if !ok {
                        let _ = log_tx.try_send(
                            "frequency correction failed: tuner would not accept it".into(),
                        );
                    } else {
                        // An accepted correction moves the LO: what follows is
                        // a different sample run even though the dial did not
                        // move. Stale queued blocks must not frame with it.
                        epoch += 1;
                        sample_pos = 0;
                    }
                    ok
                }
                Ok(Cmd::Rate(r)) => {
                    let _ = stream.deactivate(None);
                    drop(stream);
                    std::thread::sleep(std::time::Duration::from_millis(25));
                    let ok = dev.set_sample_rate(Rx, 0, r);
                    if let Err(ref e) = ok {
                        let _ = log_tx.try_send(format!("rate failed: {e}"));
                    } else {
                        rate = dev.sample_rate(Rx, 0).unwrap_or(r);
                        // New rate = new sample grid: the old positions mean
                        // nothing against it, so this starts a new epoch.
                        epoch += 1;
                        sample_pos = 0;
                        let _ = log_tx.try_send(format!("sample rate {rate:.0} S/s"));
                        std::thread::sleep(std::time::Duration::from_millis(15));
                        if let Err(e) = set_bandwidth(&dev, rate, cover_hz, &log_tx) {
                            let _ = log_tx.try_send(format!(
                                "analog bandwidth after rate change unavailable: {e}"
                            ));
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    stream = rx_stream(&dev, &caps.driver, rate, Some(&log_tx))?;
                    stream.activate(None)?;
                    ok.is_ok()
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    let _ = stream.deactivate(None);
                    return Ok(());
                }
            };
            if changed {
                publish_state(&dev, &caps, tune, agc_enabled, &event_tx);
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
                let ok = set_frequency_retrying(&dev, corrected(f, tune.ppm));
                if ok {
                    tune_fails = 0;
                    // Only now is this where the radio is.
                    tune.desired = f;
                    // Accepted tune: samples before and after belong to
                    // different RF windows — a new epoch, position restarts.
                    epoch += 1;
                    sample_pos = 0;
                    publish_state(&dev, &caps, tune, agc_enabled, &event_tx);
                } else {
                    tune_fails = tune_fails.saturating_add(1);
                    let _ = log_tx.try_send(format!(
                        "tune failed: {:.4} MHz not accepted after {TUNE_ATTEMPTS} attempts ({tune_fails}/3)",
                        f / 1e6
                    ));
                    if tune_fails >= 3 {
                        anyhow::bail!("tuner wedged after {tune_fails} failed tune attempts");
                    }
                }
            }
        }

        match stream.read(&mut [&mut buf[..]], 1_000_000) {
            Ok(0) => continue,
            Ok(n) => {
                usb_fails = 0;
                clipped += buf[..n]
                    .iter()
                    .filter(|c| c.re.abs() > 0.98 || c.im.abs() > 0.98)
                    .count() as u64;
                counted += n as u64;

                let block = IqBlock {
                    samples: Arc::from(&buf[..n]),
                    first_sample: sample_pos,
                    epoch,
                };
                sample_pos += n as u64;

                if fanout.subscriber_count() == 0 {
                    dropped += 1;
                } else {
                    lagged += u64::from(fanout.broadcast(block));
                }

                if counted >= rate as u64 {
                    let clipped_fraction = clipped as f64 / counted.max(1) as f64;
                    if let Some(ref mut cg) = clip_guard
                        && let Some(new_g) = cg.on_stats(clipped_fraction)
                    {
                        if crate::driver_log::checked(|| dev.set_gain(Rx, 0, new_g).is_ok()) {
                            let _ = log_tx.try_send(format!(
                                "gain {new_g:.1} dB (clip {clipped_fraction:.4})"
                            ));
                            publish_state(&dev, &caps, tune, agc_enabled, &event_tx);
                        } else {
                            // Counted by the server as a control fault; two
                            // in a window reopen the device, which is what
                            // clears a stalled tuner bus.
                            let _ = log_tx.try_send(format!(
                                "gain {new_g:.1} dB failed: tuner did not take the write"
                            ));
                        }
                    }
                    let _ = event_tx.try_send(Event::StreamStats {
                        dropped_blocks: dropped,
                        lagged_deliveries: lagged,
                        clipped_fraction,
                    });
                    clipped = 0;
                    counted = 0;
                }
            }
            Err(e) => {
                usb_fails += 1;
                let _ = log_tx.try_send(format!("read error ({usb_fails}/3): {e}"));
                if usb_fails >= 3 {
                    let _ = stream.deactivate(None);
                    anyhow::bail!("dongle not responding after {usb_fails} read errors: {e}");
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}

/// Manual overall gain, clamped to what this tuner can actually do.
///
/// RTL-SDR reports a wide Soapy range (96 dB on the E4000) but librtlsdr
/// saturates earlier (52 dB on that chip, 49.6 dB on the R820T). Asking
/// above the real ceiling is treated as "maximum".
fn apply_overall_gain(
    dev: &soapysdr::Device,
    caps: &Capabilities,
    agc_enabled: &mut bool,
    want: f64,
    log: &SyncSender<String>,
) -> bool {
    if caps.hardware_agc && *agc_enabled {
        for _ in 0..3 {
            if dev.set_gain_mode(Rx, 0, false).is_ok() {
                *agc_enabled = false;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    let g = match caps.gain {
        GainControl::Overall { min, max } => want.clamp(min, max),
        GainControl::Sdrplay { .. } => want,
    };
    let mut ok = false;
    for attempt in 0..GAIN_ATTEMPTS {
        // The driver reports success whether or not the I2C write took;
        // the stderr watcher is what says it did.
        if crate::driver_log::checked(|| dev.set_gain(Rx, 0, g).is_ok()) {
            ok = true;
            break;
        }
        if attempt + 1 < GAIN_ATTEMPTS {
            std::thread::sleep(GAIN_RETRY_DELAY * (attempt + 1));
        }
    }
    if ok {
        let got = dev.gain(Rx, 0).unwrap_or(g);
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

/// Attempts per tuner write, and the pause between them. The pause grows
/// per attempt: a stalled control endpoint that fails at 6 ms sometimes
/// clears by 30, and a write that still fails after that is reported so
/// the receiver can be reopened, which is what does clear it.
const TUNE_ATTEMPTS: u32 = 4;
const TUNE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(8);
const GAIN_ATTEMPTS: u32 = 4;
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
        ..State::default()
    };
    match caps.gain {
        GainControl::Overall { .. } => state.overall_gain = dev.gain(Rx, 0).ok(),
        GainControl::Sdrplay { .. } => {
            state.rfgr = dev.gain_element(Rx, 0, "RFGR").ok();
            state.ifgr = dev.gain_element(Rx, 0, "IFGR").ok();
        }
    }
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
        choose_bandwidth(&options, rate, cover_hz)
    };
    dev.set_bandwidth(Rx, 0, want)?;
    let actual = dev.bandwidth(Rx, 0).unwrap_or(want);
    let _ = log_tx.try_send(format!("analog bandwidth {actual:.0} Hz"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut g = ClipGuard::new(30.0, 0.0, 49.6);
        assert!(g.on_stats(0.0).is_none());
        assert_eq!(g.on_stats(0.01), None, "one hot second is not enough");
        assert_eq!(g.on_stats(0.01), Some(28.0));
        // Eight clean seconds creep 1 dB back toward 30.
        for _ in 0..7 {
            assert!(g.on_stats(0.0).is_none());
        }
        assert_eq!(g.on_stats(0.0), Some(29.0));
        for _ in 0..8 {
            g.on_stats(0.0);
        }
        assert_eq!(g.current, 30.0, "must not exceed the configured ceiling");
    }

    /// A hand-set gain is replaced by a fresh guard at that gain (Cmd::Gain
    /// rebuilds it), so the new ceiling is simply the new guard's start.
    #[test]
    fn clip_guard_never_exceeds_a_new_lower_ceiling() {
        let mut g = ClipGuard::new(20.0, 0.0, 49.6);
        for _ in 0..20 {
            assert!(g.on_stats(0.0).is_none());
        }
        assert_eq!(g.current, 20.0);
    }

    /// The auto-gain guard starts below the maximum, climbs to it on a quiet
    /// band, and comes back down as soon as the ADC clips.
    #[test]
    fn auto_gain_guard_climbs_to_max_and_backs_off_on_clipping() {
        let start = auto_gain_start(0.0, 49.6);
        assert!((start - 19.84).abs() < 0.01);
        let mut g = ClipGuard::auto(start, 0.0, 49.6);
        let mut steps = 0;
        for _ in 0..400 {
            if g.on_stats(0.0).is_some() {
                steps += 1;
            }
        }
        assert!(steps > 0 && (g.current - 49.6).abs() < 0.05, "climbed to {}", g.current);
        assert!(g.on_stats(0.01).is_none());
        assert_eq!(g.on_stats(0.01), Some(47.6));
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
}
