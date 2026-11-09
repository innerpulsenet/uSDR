//! FT8 channels cut from the wide span, beside whatever the main receiver is
//! doing.
//!
//! The span is 8 MHz wide, so one tuner position can hold several FT8 dials at
//! once. Each channel here mixes its dial to baseband, decimates, demodulates
//! and fills its own slot decoder, on a thread of its own: the radio loop only
//! copies a block into a bounded queue and never waits. A channel runs whenever
//! the tuner happens to cover its dial.
//!
//! Every slot's result is kept for comparing against the main decoder. With
//! `USDR_WIDE_LOG` set the decodes also go where the main decoder's go, except
//! while the main decoder is on the same dial, which then speaks for it. Each
//! channel keeps its own listening record, marked `wideband`, so its airtime is
//! never mistaken for the receiver's.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};

use num_complex::Complex32;
use scannerd_dsp::{HalfbandChain, Nco};
use scannerd_engine::ft8::{Protocol, SlotDecoder};
use scannerd_engine::ssb::{Sideband, SsbDemod};
use serde::Serialize;
use serde_json::{Value, json};

use tokio::sync::broadcast::Sender;

use crate::sdr::{SdrEvent, SdrMode, ft8_event};
use crate::stationlog::{Listener, RadioNow, StationLog};

/// Marks a decode as from a channel here, not the main receiver.
pub const CHANNEL_FIELD: &str = "wideChannel";

/// Decimate until the rate is under this. FT8 needs 3.1 kHz of it.
const CHAIN_RATE_MAX: f64 = 96_000.0;
/// Flat and alias-free this far either side of the dial.
const CHAIN_BAND_HZ: f64 = 6_000.0;
/// How much of the span is good enough to decode from, either side of center.
const USABLE: f64 = 0.46;
/// The audio a channel searches sits this high above its dial, at most.
const TOP_HZ: f64 = 3_100.0;
/// Blocks waiting for the channels. A long stall drops blocks, and counts them.
const QUEUE: usize = 64;
const KEEP_SLOTS: usize = 4_000;
/// The longest a channel's listening record runs before starting a new one.
const RECORD_S: u64 = 300;

struct Block {
    iq: Vec<Complex32>,
    rate: f64,
    center_hz: f64,
    ms: u64,
    /// The dial the main FT8 decoder is on, if it is on one.
    main_dial: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SlotResult {
    pub dial_hz: u64,
    pub slot_ms: u64,
    pub center_hz: u64,
    pub decodes: Vec<String>,
}

static RESULTS: OnceLock<Mutex<VecDeque<SlotResult>>> = OnceLock::new();
static DROPPED: AtomicU64 = AtomicU64::new(0);
static LOG: OnceLock<Arc<StationLog>> = OnceLock::new();

/// Where the channels' listening records go.
pub fn attach(log: Arc<StationLog>) {
    let _ = LOG.set(log);
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn results() -> &'static Mutex<VecDeque<SlotResult>> {
    RESULTS.get_or_init(|| Mutex::new(VecDeque::with_capacity(KEEP_SLOTS)))
}

/// Whether a span holds this dial and all the audio above it.
fn hears(dial_hz: u64, center_hz: f64, rate: f64) -> bool {
    let half = rate * USABLE;
    let dial = dial_hz as f64;
    (dial - center_hz).abs() < half && (dial + TOP_HZ - center_hz).abs() < half
}

struct Channel {
    dial_hz: u64,
    center_hz: f64,
    nco: Nco,
    mixed: Vec<Complex32>,
    chain: HalfbandChain,
    baseband: Vec<Complex32>,
    demod: SsbDemod,
    audio: Vec<f32>,
    linear: Vec<f32>,
    slots: SlotDecoder,
}

impl Channel {
    fn new(dial_hz: u64, center_hz: f64, rate: f64) -> Self {
        let mut stages = 0_u32;
        while rate / f64::from(1_u32 << stages) > CHAIN_RATE_MAX {
            stages += 1;
        }
        let chain = HalfbandChain::new(rate, CHAIN_BAND_HZ, stages);
        let fs = chain.fs_out();
        let mut nco = Nco::new();
        nco.set_freq(dial_hz as f64 - center_hz, rate);
        let (low, high) = SdrMode::Ft8.filter_edges();
        let demod = SsbDemod::with_shape(fs, Sideband::Upper, low.abs().min(high.abs()), low.abs().max(high.abs()), SdrMode::Ft8.filter_shape());
        let mut slots = SlotDecoder::new(Protocol::Ft8, fs);
        slots.set_band(low.abs().min(high.abs()), low.abs().max(high.abs()));
        Self { dial_hz, center_hz, nco, mixed: Vec::new(), chain, baseband: Vec::new(), demod, audio: Vec::new(), linear: Vec::new(), slots }
    }

    fn process(&mut self, iq: &[Complex32], ms: u64) {
        self.nco.mix(iq, &mut self.mixed);
        self.chain.process(&self.mixed, &mut self.baseband);
        self.demod.process_with_linear(&self.baseband, &mut self.audio, &mut self.linear);
        self.slots.push(&self.linear, ms);
    }

    /// Finished slots, with each decode as the log carries it.
    fn collect(&mut self) -> Vec<(SlotResult, Vec<scannerd_engine::DecodeEvent>)> {
        self.slots
            .take()
            .into_iter()
            .map(|(slot_ms, decodes)| {
                let events = decodes
                    .iter()
                    .map(|d| {
                        let mut event = ft8_event(slot_ms, d);
                        event.fields.insert(CHANNEL_FIELD.to_string(), "1".to_string());
                        event
                    })
                    .collect();
                let result = SlotResult { dial_hz: self.dial_hz, slot_ms, center_hz: self.center_hz as u64, decodes: decodes.into_iter().map(|d| d.text).collect() };
                (result, events)
            })
            .collect()
    }
}

fn run(rx: Receiver<Block>, dials: Vec<u64>, events: Sender<SdrEvent>, logging: bool) {
    // One record per dial, numbered well clear of the main receiver's.
    let mut records: Vec<(u64, Listener)> = dials.iter().enumerate().map(|(i, d)| (*d, Listener::numbered_from((i as u64 + 1) << 40))).collect();
    let mut last_second = 0;
    let mut channels: Vec<Channel> = Vec::new();
    // Channels from before a retune, kept until their last decode comes back.
    let mut retired: Vec<Channel> = Vec::new();
    let mut key = None;
    while let Ok(block) = rx.recv() {
        if key != Some((block.rate, block.center_hz)) {
            key = Some((block.rate, block.center_hz));
            retired.append(&mut channels);
            channels = dials.iter().filter(|d| hears(**d, block.center_hz, block.rate)).map(|d| Channel::new(*d, block.center_hz, block.rate)).collect();
        }
        for channel in &mut channels {
            channel.process(&block.iq, block.ms);
        }
        let done: Vec<_> = channels.iter_mut().chain(retired.iter_mut()).flat_map(Channel::collect).collect();
        retired.retain(|c| c.slots.busy());
        for (slot, slot_events) in done {
            if logging && block.main_dial != Some(slot.dial_hz) {
                let record = records.iter_mut().find(|(d, _)| *d == slot.dial_hz).map(|(_, l)| l);
                if let Some(record) = record {
                    for event in slot_events {
                        record.decode(event.valid);
                        let _ = events.send(SdrEvent::Decode { inspect_hz: slot.dial_hz as f64, event });
                    }
                }
            }
            let mut kept = results().lock().expect("wideband results");
            if kept.len() == KEEP_SLOTS {
                kept.pop_front();
            }
            kept.push_back(slot);
        }
        let now = unix_now();
        if logging && now != last_second {
            last_second = now;
            for (dial, record) in &mut records {
                let listening = block.main_dial != Some(*dial) && channels.iter().any(|c| c.dial_hz == *dial);
                // Ended every few minutes: the uplink sends these rows in order, and one
                // that stayed open would hold back everything logged after it.
                let radio = (listening && now % RECORD_S != 0).then_some(RadioNow { dial_hz: *dial as f64, mode: SdrMode::Ft8, rfgr_db: None, cause: "wideband" });
                for segment in record.tick(now, radio, true) {
                    if let Some(log) = LOG.get() {
                        log.add_segment(segment);
                    }
                }
            }
        }
    }
}

/// The radio loop's end of the queue.
pub struct Feed {
    tx: SyncSender<Block>,
    dials: Vec<u64>,
}

impl Feed {
    pub fn feed(&self, iq: &[Complex32], rate: f64, center_hz: f64, ms: u64, main_dial: Option<u64>) {
        if !self.dials.iter().any(|d| hears(*d, center_hz, rate)) {
            return;
        }
        let block = Block { iq: iq.to_vec(), rate, center_hz, ms, main_dial };
        if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) = self.tx.try_send(block) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Start the channels named in `USDR_WIDE_FT8`, dials in Hz separated by
/// commas. Unset, nothing runs and the radio loop does no extra work.
pub fn from_env(events: Sender<SdrEvent>) -> Option<Feed> {
    let text = std::env::var("USDR_WIDE_FT8").ok()?;
    let dials: Vec<u64> = text.split(',').filter_map(|d| d.trim().parse().ok()).filter(|d| *d < 30_000_000).collect();
    if dials.is_empty() {
        return None;
    }
    let logging = std::env::var_os("USDR_WIDE_LOG").is_some();
    let (tx, rx) = sync_channel(QUEUE);
    let worker = dials.clone();
    match std::thread::Builder::new().name("wideband".into()).spawn(move || run(rx, worker, events, logging)) {
        Ok(_) => {
            eprintln!("wideband: {} on {dials:?}", if logging { "logging channels" } else { "shadow channels" });
            Some(Feed { tx, dials })
        }
        Err(e) => {
            eprintln!("wideband: could not start: {e}");
            None
        }
    }
}

/// What the shadow channels have decoded, for comparing with the main decoder.
pub fn snapshot() -> Value {
    let kept = results().lock().expect("wideband results");
    json!({ "dropped_blocks": DROPPED.load(Ordering::Relaxed), "slots": kept.iter().collect::<Vec<_>>() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dial_needs_its_audio_inside_the_usable_span() {
        assert!(hears(3_573_000, 7_174_000.0, 8e6));
        assert!(hears(10_136_000, 7_174_000.0, 8e6));
        assert!(!hears(14_074_000, 7_174_000.0, 8e6), "the far side of the span");
        assert!(!hears(3_450_000, 7_174_000.0, 8e6));
    }

    #[test]
    fn a_signal_off_center_in_the_span_is_decoded() {
        let rate = 250_000.0;
        let offset = 30_000.0;
        let center = 7_044_000.0;
        let dial = 7_074_000_u64;
        let start_ms = 1_800_000_000_000_u64;
        let audio = scannerd_engine::ft8::synthesize(Protocol::Ft8, "CQ K1ABC FN42", 1_500.0, 0.5, 15.0).expect("a message");
        let mut channel = Channel::new(dial, center, rate);
        let total = (rate * 16.0) as usize;
        let mut sent = 0;
        let mut block = Vec::with_capacity(16_384);
        while sent < total {
            block.clear();
            for n in sent..(sent + 16_384).min(total) {
                let t = n as f64 / rate;
                let at = t * 12_000.0;
                let i = at as usize;
                let a = audio.get(i).copied().unwrap_or(0.0) as f64;
                let b = audio.get(i + 1).copied().unwrap_or(0.0) as f64;
                let v = a + (b - a) * (at - i as f64);
                let phase = std::f64::consts::TAU * offset * t;
                block.push(Complex32::new((v * phase.cos()) as f32, (v * phase.sin()) as f32));
            }
            sent += block.len();
            channel.process(&block, start_ms + (sent as f64 / rate * 1_000.0) as u64);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        let mut got = Vec::new();
        while got.is_empty() && std::time::Instant::now() < deadline {
            got = channel.collect().into_iter().map(|(slot, events)| (slot, events.len())).collect();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(got[0].0.dial_hz, dial);
        assert!(got[0].1 > 0, "the slot's decodes come with their events");
        assert!(got.iter().flat_map(|(s, _)| &s.decodes).any(|t| t.contains("K1ABC")), "{got:?}");
    }
}
