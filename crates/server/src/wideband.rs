//! Decoder channels cut from the wide span, beside whatever the main receiver
//! is doing.
//!
//! The span is 8 MHz wide, so one tuner position can hold many digital dials at
//! once. Each channel mixes its dial to baseband, decimates, demodulates and
//! feeds its own decoder, on worker threads of their own: the radio loop only
//! hands over a shared block through a bounded queue and never waits. A channel
//! runs whenever the tuner happens to cover its dial.
//!
//! `USDR_WIDE` lists the channels, for example `FT8+7074000,WSPR?7038600`, and
//! `catalog` stands for every dial the monitoring schedule knows, radiofax charts
//! included, as logging channels. Only the ones the span holds run. A `+`
//! channel logs its decodes where the main decoder's go, except while the main
//! decoder is on the same mode and dial, which then speaks for it. A `?` channel
//! is a shadow, whose decodes are kept only for comparing with the main decoder.
//! Each logging channel keeps its own listening record, marked `wideband`, so its
//! airtime is never mistaken for the receiver's.
//!
//! SSTV below 10 MHz is sent on the lower sideband, as amateur voice is, and a
//! channel there demodulates it that way. Its pictures go to the approval queue
//! like the main receiver's, unless the main receiver was on the same dial for
//! the whole picture and keeps its own. Radiofax channels (`WEFAX+9108100`, the
//! dial 1.9 kHz below the listed frequency) start a chart on its own start tone
//! and keep it the same way; while one is mid-chart, [`chart_busy`] tells the
//! schedule not to move the span.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};

use num_complex::Complex32;
use scannerd_dsp::{HalfbandChain, Nco};
use scannerd_engine::DecodeEvent;
use scannerd_engine::beacon::{BEACONS, BANDS_HZ, BeaconDetector, Heard};
use scannerd_engine::ft8::SlotDecoder;
use scannerd_engine::ssb::{Sideband, SsbDemod};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::broadcast::{self, Sender};

use crate::hfdata::HfDecoder;
use crate::sdr::{SdrEvent, SdrMode, SdrStatus, ft8_event};
use crate::stationlog::{Listener, RadioNow, StationLog, monitored_mode, observe};

/// Decimate until the rate is under this. No mode here needs more than a few kHz.
const CHAIN_RATE_MAX: f64 = 96_000.0;
/// Flat and alias-free this far either side of the dial.
const CHAIN_BAND_HZ: f64 = 6_000.0;
/// How much of the span is good enough to decode from, either side of center.
const USABLE: f64 = 0.46;
/// Blocks waiting for a worker. A long stall drops blocks, and counts them.
const QUEUE: usize = 64;
const KEEP_RESULTS: usize = 12_000;
/// The longest a channel's listening record runs before starting a new one.
const RECORD_S: u64 = 300;
/// Worker threads, each with its share of the channels.
const WORKERS: usize = 2;
/// Samples of the span each channel takes at a time. A whole delivery is
/// 850 KB at 8 MS/s, and every channel mixing it into scratch of its own
/// kept tens of megabytes moving through the cache: twelve channels cost a
/// third more than in tiles this size (`bench_chain`).
const TILE: usize = 4_096;

/// Dials no further apart than this share a front end: one mixer and the
/// first halvings for the group, then a short chain each. Every band's FT8
/// and WSPR dials are such a pair (10 m's are the widest, 50.6 kHz), and
/// the stages at the span's full rate are most of what a channel costs.
const GROUP_SPREAD_HZ: u64 = 60_000;
/// A group's own rate is at least this many times the band it carries.
const GROUP_OVERSAMPLE: f64 = 8.0;

/// Marks a decode as from a channel here, not the main receiver.
pub const CHANNEL_FIELD: &str = "wideChannel";

struct Block {
    iq: Arc<Vec<Complex32>>,
    rate: f64,
    center_hz: f64,
    ms: u64,
    /// What the main receiver is decoding, if it is a mode channels can also run.
    main: Option<(SdrMode, u64)>,
}

/// One channel's result: a whole slot's decodes, or a single message.
#[derive(Clone, Debug, Serialize)]
pub struct ChannelResult {
    pub mode: &'static str,
    pub dial_hz: u64,
    pub at_ms: u64,
    pub center_hz: u64,
    pub decodes: Vec<String>,
    pub idents: Vec<String>,
}

static RESULTS: OnceLock<Mutex<VecDeque<ChannelResult>>> = OnceLock::new();
static DROPPED: AtomicU64 = AtomicU64::new(0);
/// Per worker, whether one of its radiofax channels is in the middle of a chart.
static CHARTING: [AtomicBool; WORKERS] = [const { AtomicBool::new(false) }; WORKERS];
static LOG: OnceLock<Arc<StationLog>> = OnceLock::new();

/// Where the channels' listening records go.
pub fn attach(log: Arc<StationLog>) {
    let _ = LOG.set(log);
}

fn results() -> &'static Mutex<VecDeque<ChannelResult>> {
    RESULTS.get_or_init(|| Mutex::new(VecDeque::with_capacity(KEEP_RESULTS)))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Spec {
    mode: SdrMode,
    dial_hz: u64,
    logs: bool,
}

/// Below this, picture modes go out on the lower sideband.
const LOWER_SIDEBAND_BELOW_HZ: u64 = 10_000_000;

impl Spec {
    fn new(mode: SdrMode, dial_hz: u64, logs: bool) -> Self {
        Spec { mode, dial_hz, logs }
    }

    /// The data modes here are all upper sideband except SSTV low down.
    fn sideband(self) -> Sideband {
        if self.mode == SdrMode::Sstv && self.dial_hz < LOWER_SIDEBAND_BELOW_HZ { Sideband::Lower } else { Sideband::Upper }
    }

    /// How far from the dial this mode's audio reaches, on its sideband's side.
    fn top_hz(self) -> f64 {
        let (low, high) = self.mode.filter_edges();
        f64::from(low.abs().max(high.abs()))
    }

    /// Whether a span holds this dial and all the audio beside it.
    fn heard_in(self, center_hz: f64, rate: f64) -> bool {
        let half = rate * USABLE;
        let dial = self.dial_hz as f64;
        let edge = match self.sideband() {
            Sideband::Lower => dial - self.top_hz(),
            Sideband::Upper => dial + self.top_hz(),
        };
        (dial - center_hz).abs() < half && (edge - center_hz).abs() < half
    }
}

fn parse_mode(name: &str) -> Option<SdrMode> {
    Some(match name {
        "FT8" => SdrMode::Ft8,
        "FT4" => SdrMode::Ft4,
        "WSPR" => SdrMode::Wspr,
        "JS8" => SdrMode::Js8,
        "DSC" => SdrMode::Dsc,
        "HFDL" => SdrMode::Hfdl,
        "SELCAL" => SdrMode::Selcal,
        "SSTV" => SdrMode::Sstv,
        "WEFAX" => SdrMode::Wefax,
        "BEACON" => SdrMode::Beacon,
        _ => return None,
    })
}

/// `FT8+7074000,WSPR?7038600,catalog`, with `catalog` standing for `catalog`'s
/// specs. A dial named twice keeps its first spec. Anything unreadable, or above
/// HF, is left out and returned beside the specs.
fn parse_specs(text: &str, catalog: &[Spec]) -> (Vec<Spec>, Vec<String>) {
    let (mut specs, mut ignored): (Vec<Spec>, Vec<String>) = (Vec::new(), Vec::new());
    fn add(spec: Spec, specs: &mut Vec<Spec>) {
        if !specs.iter().any(|s| s.mode == spec.mode && s.dial_hz == spec.dial_hz) {
            specs.push(spec);
        }
    }
    let items: Vec<&str> = text.split(',').map(str::trim).filter(|i| !i.is_empty()).collect();
    // Named channels first, so a shadow named on a catalog dial stays a shadow.
    for item in items.iter().filter(|i| !i.eq_ignore_ascii_case("catalog")) {
        let spec = item.find(['+', '?']).and_then(|at| {
            let dial_hz: u64 = item[at + 1..].parse().ok()?;
            (dial_hz < 30_000_000).then_some(Spec::new(parse_mode(&item[..at])?, dial_hz, item.as_bytes()[at] == b'+'))
        });
        match spec {
            Some(spec) => add(spec, &mut specs),
            None => ignored.push((*item).to_string()),
        }
    }
    if items.iter().any(|i| i.eq_ignore_ascii_case("catalog")) {
        for spec in catalog {
            add(*spec, &mut specs);
        }
    }
    (specs, ignored)
}

/// Every dial the schedule knows, as logging channels.
fn catalog_specs() -> Vec<Spec> {
    crate::schedule::Catalog::builtin().channel_targets().into_iter().map(|t| Spec::new(t.mode, t.dial_hz, true)).collect()
}

/// The channels `USDR_WIDE` asks for, and what in it could not be read.
fn specs_from_env() -> Option<(Vec<Spec>, Vec<String>)> {
    let text = std::env::var("USDR_WIDE").ok()?;
    let catalog = if text.split(',').any(|i| i.trim().eq_ignore_ascii_case("catalog")) { catalog_specs() } else { Vec::new() };
    Some(parse_specs(&text, &catalog))
}

/// The specs in dial order, cut into runs no wider than [`GROUP_SPREAD_HZ`].
fn groups(specs: &[Spec]) -> Vec<Vec<Spec>> {
    let mut sorted = specs.to_vec();
    sorted.sort_by_key(|s| s.dial_hz);
    let mut out: Vec<Vec<Spec>> = Vec::new();
    for spec in sorted {
        match out.last_mut() {
            Some(g) if spec.dial_hz - g[0].dial_hz <= GROUP_SPREAD_HZ => g.push(spec),
            _ => out.push(vec![spec]),
        }
    }
    out
}

/// Halvings from `rate` until it is under [`CHAIN_RATE_MAX`].
fn stages_for(rate: f64) -> u32 {
    let mut stages = 0_u32;
    while rate / f64::from(1_u32 << stages) > CHAIN_RATE_MAX {
        stages += 1;
    }
    stages
}

/// A group's shared front end: the span mixed to the group's middle and
/// halved as far as the group's width allows.
struct Front {
    center_hz: f64,
    nco: Nco,
    mixed: Vec<Complex32>,
    chain: HalfbandChain,
    part: Vec<Complex32>,
    out: Vec<Complex32>,
}

impl Front {
    /// None when the group is too wide for sharing to save a stage.
    fn new(group: &[Spec], center_hz: f64, rate: f64) -> Option<Self> {
        let (low, high) = (group.first()?.dial_hz as f64, group.last()?.dial_hz as f64);
        let middle = (low + high) / 2.0;
        let band = (high - low) / 2.0 + CHAIN_BAND_HZ;
        let mut stages = 0_u32;
        while stages < stages_for(rate) && rate / f64::from(1_u32 << (stages + 1)) >= GROUP_OVERSAMPLE * band {
            stages += 1;
        }
        if stages < 2 {
            return None;
        }
        let mut nco = Nco::new();
        nco.set_freq(middle - center_hz, rate);
        Some(Self { center_hz: middle, nco, mixed: Vec::new(), chain: HalfbandChain::new(rate, band, stages), part: Vec::new(), out: Vec::new() })
    }

    fn ingest(&mut self, iq: &[Complex32]) {
        self.nco.mix(iq, &mut self.mixed);
        self.chain.process(&self.mixed, &mut self.part);
        self.out.extend_from_slice(&self.part);
    }
}

/// The fronts and channels for the specs a span holds. A channel in a
/// group names its front; the rest take the span itself.
fn build(specs: &[Spec], center_hz: f64, rate: f64) -> (Vec<Front>, Vec<Channel>) {
    let (mut fronts, mut channels) = (Vec::new(), Vec::new());
    let heard: Vec<Spec> = specs.iter().copied().filter(|s| s.heard_in(center_hz, rate)).collect();
    for group in groups(&heard) {
        let front = if group.len() > 1 { Front::new(&group, center_hz, rate) } else { None };
        match front {
            Some(front) => {
                let fs = front.chain.fs_out();
                let stages = stages_for(rate) - front.chain.taps().len() as u32;
                channels.extend(group.iter().filter_map(|s| Channel::with_chain(*s, center_hz, front.center_hz, fs, stages, Some(fronts.len()))));
                fronts.push(front);
            }
            None => channels.extend(group.iter().filter_map(|s| Channel::new(*s, center_hz, rate))),
        }
    }
    (fronts, channels)
}

/// One block of the span through every front and channel.
fn process_block(fronts: &mut [Front], channels: &mut [Channel], iq: &[Complex32], ms: u64, sink: &Sender<SdrEvent>, status: &Arc<Mutex<SdrStatus>>) {
    for front in fronts.iter_mut() {
        front.out.clear();
        for tile in iq.chunks(TILE) {
            front.ingest(tile);
        }
    }
    // A channel at a time through the block's tiles, so its own scratch
    // stays in the cache from one tile to the next: on the radio host that
    // order measured a tenth cheaper than a tile through every channel.
    for channel in channels.iter_mut() {
        match channel.front {
            Some(f) => channel.ingest(&fronts[f].out),
            None => {
                for tile in iq.chunks(TILE) {
                    channel.ingest(tile);
                }
            }
        }
        channel.finish(ms, sink, status);
    }
}

enum Decoder {
    Slots(Box<SlotDecoder>),
    Hf(Box<HfDecoder>),
    Beacon(Box<BeaconDetector>),
}

/// A beacon channel's dial sits this far below the beacon frequency, so the
/// carrier is a Morse pitch in the middle of Morse's passband.
pub const BEACON_PITCH_HZ: u64 = 700;

/// A beacon heard, as the log and the site take it.
fn beacon_event(h: &Heard) -> DecodeEvent {
    let b = &BEACONS[h.beacon];
    let mhz = BANDS_HZ[h.band] as f64 / 1e6;
    let power = ["callsign only", "100 W", "down to 10 W", "down to 1 W", "down to 100 mW"][usize::from(h.steps.min(4))];
    let mut fields = std::collections::BTreeMap::new();
    let mut put = |k: &str, v: String| {
        fields.insert(k.to_string(), v);
    };
    put("call", b.call.into());
    put("place", b.place.into());
    put("grid", b.grid.into());
    put("freqHz", BANDS_HZ[h.band].to_string());
    put("slot", h.slot_utc.to_string());
    put("snr", format!("{:.0}", h.snr_db));
    put("steps", h.steps.to_string());
    for (i, d) in h.dashes_db.iter().enumerate() {
        if let Some(d) = d {
            put(&format!("dash{}", i + 1), format!("{d:.0}"));
        }
    }
    put("df", format!("{:.1}", h.offset_hz));
    put("lateMs", h.late_ms.to_string());
    put("score", format!("{:.0}", h.score));
    DecodeEvent {
        protocol: "BEACON".into(),
        kind: "heard".into(),
        summary: format!("{} · {mhz:.3} MHz · {:+.0} dB · {power}", b.call, h.snr_db),
        valid: true,
        fields,
    }
}

struct Channel {
    spec: Spec,
    center_hz: f64,
    nco: Nco,
    mixed: Vec<Complex32>,
    chain: HalfbandChain,
    part: Vec<Complex32>,
    baseband: Vec<Complex32>,
    demod: SsbDemod,
    audio: Vec<f32>,
    linear: Vec<f32>,
    decoder: Decoder,
    /// The shared front end this channel takes its samples from, if any.
    front: Option<usize>,
}

impl Channel {
    /// A channel cut straight from the span.
    fn new(spec: Spec, center_hz: f64, rate: f64) -> Option<Self> {
        Self::with_chain(spec, center_hz, center_hz, rate, stages_for(rate), None)
    }

    /// A channel cut from samples at `rate` centered on `input_hz`: the
    /// span, or a group's front end. `center_hz` is the span's, for the
    /// record.
    fn with_chain(spec: Spec, center_hz: f64, input_hz: f64, rate: f64, stages: u32, front: Option<usize>) -> Option<Self> {
        let chain = HalfbandChain::new(rate, CHAIN_BAND_HZ, stages);
        let fs = chain.fs_out();
        let mut nco = Nco::new();
        nco.set_freq(spec.dial_hz as f64 - input_hz, rate);
        let (low, high) = spec.mode.filter_edges();
        let (low, high) = (low.abs().min(high.abs()), low.abs().max(high.abs()));
        let demod = SsbDemod::with_shape(fs, spec.sideband(), low, high, spec.mode.filter_shape());
        let decoder = match spec.mode.ft8_protocol() {
            None if spec.mode == SdrMode::Beacon => {
                let band = scannerd_engine::beacon::band_of(spec.dial_hz + BEACON_PITCH_HZ)?;
                Decoder::Beacon(Box::new(BeaconDetector::new(fs as f32, band, BEACON_PITCH_HZ as f32)))
            }
            Some(protocol) => {
                let mut slots = SlotDecoder::new(protocol, fs);
                slots.set_band(low, high);
                // USDR_FT8_DUMP=<dir> keeps every slot's audio under
                // <dir>/<dial>, for decoding the same slots again offline.
                if let Some(dir) = std::env::var_os("USDR_FT8_DUMP") {
                    let dir = std::path::Path::new(&dir).join(spec.dial_hz.to_string());
                    if std::fs::create_dir_all(&dir).is_ok() {
                        slots.set_dump(Some(dir));
                    }
                }
                Decoder::Slots(Box::new(slots))
            }
            None => {
                let mut hf = HfDecoder::new(spec.mode, fs)?;
                hf.set_band(low, high);
                hf.set_channel();
                Decoder::Hf(Box::new(hf))
            }
        };
        Some(Self { spec, center_hz, nco, mixed: Vec::new(), chain, part: Vec::new(), baseband: Vec::new(), demod, audio: Vec::new(), linear: Vec::new(), decoder, front })
    }

    /// A tile of the span, mixed and decimated onto the block's baseband.
    /// The mixer and the filters carry their state across calls, so the
    /// result is the same however the span is cut.
    fn ingest(&mut self, iq: &[Complex32]) {
        self.nco.mix(iq, &mut self.mixed);
        self.chain.process(&self.mixed, &mut self.part);
        self.baseband.extend_from_slice(&self.part);
    }

    /// One block of the span. Decodes from audio-driven decoders go to `sink`.
    #[cfg(test)]
    fn process(&mut self, iq: &[Complex32], ms: u64, sink: &Sender<SdrEvent>, status: &Arc<Mutex<SdrStatus>>) {
        for tile in iq.chunks(TILE) {
            self.ingest(tile);
        }
        self.finish(ms, sink, status);
    }

    /// The block's baseband, demodulated and decoded. Decodes from
    /// audio-driven decoders go to `sink`.
    fn finish(&mut self, ms: u64, sink: &Sender<SdrEvent>, status: &Arc<Mutex<SdrStatus>>) {
        self.demod.process_with_linear(&self.baseband, &mut self.audio, &mut self.linear);
        match &mut self.decoder {
            Decoder::Slots(slots) => slots.push(&self.linear, ms),
            Decoder::Hf(hf) => hf.push(&self.linear, self.spec.dial_hz as f64, sink, status),
            Decoder::Beacon(b) => b.push(&self.linear, ms),
        }
        self.baseband.clear();
    }

    /// Finished FT8 or FT4 slots, each decode as the log carries it.
    fn slots_done(&mut self) -> Vec<(u64, Vec<DecodeEvent>)> {
        match &mut self.decoder {
            Decoder::Slots(slots) => slots.take().into_iter().map(|(slot_ms, decodes)| (slot_ms, decodes.iter().map(|d| ft8_event(slot_ms, d)).collect())).collect(),
            // Beacons heard, each as its slot's one decode.
            Decoder::Beacon(b) => b.take().into_iter().map(|h| (h.slot_utc * 1_000, vec![beacon_event(&h)])).collect(),
            Decoder::Hf(_) => Vec::new(),
        }
    }

    fn busy(&self) -> bool {
        matches!(&self.decoder, Decoder::Slots(slots) if slots.busy())
    }

    /// A radiofax channel between a chart's start and its end.
    fn charting(&self) -> bool {
        self.spec.mode == SdrMode::Wefax && matches!(&self.decoder, Decoder::Hf(hf) if hf.picture_busy())
    }

    /// Pictures are this channel's to keep unless it only shadows, or the main
    /// receiver is decoding the same mode on the same dial.
    fn follow_main(&mut self, main: Option<(SdrMode, u64)>) {
        if let Decoder::Hf(hf) = &mut self.decoder {
            hf.set_kept_elsewhere(!self.spec.logs || main == Some((self.spec.mode, self.spec.dial_hz)));
        }
    }
}

/// Keep a result, and send its decodes on if this channel logs.
fn deliver(spec: Spec, center_hz: f64, at_ms: u64, group: Vec<DecodeEvent>, main: Option<(SdrMode, u64)>, events: &Sender<SdrEvent>, records: &mut [(Spec, Listener)]) {
    let name = monitored_mode(spec.mode).unwrap_or("?");
    let idents = group.iter().filter_map(|e| observe(spec.dial_hz as f64, e, "", at_ms / 1_000).and_then(|o| o.ident)).collect();
    let decodes = group.iter().map(|e| e.summary.clone()).collect();
    if spec.logs && main != Some((spec.mode, spec.dial_hz)) {
        let mut record = records.iter_mut().find(|(s, _)| *s == spec).map(|(_, l)| l);
        for mut event in group {
            if let Some(record) = record.as_mut() {
                record.decode(event.valid);
            }
            event.fields.insert(CHANNEL_FIELD.to_string(), "1".to_string());
            let _ = events.send(SdrEvent::Decode { inspect_hz: spec.dial_hz as f64, event });
        }
    }
    let mut kept = results().lock().expect("wideband results");
    if kept.len() == KEEP_RESULTS {
        kept.pop_front();
    }
    kept.push_back(ChannelResult { mode: name, dial_hz: spec.dial_hz, at_ms, center_hz: center_hz as u64, decodes, idents });
}

fn run(rx: Receiver<Arc<Block>>, specs: Vec<Spec>, events: Sender<SdrEvent>, seq_base: u64, worker: usize) {
    // One record per logging channel, numbered well clear of the main receiver's.
    let mut records: Vec<(Spec, Listener)> =
        specs.iter().filter(|s| s.logs).enumerate().map(|(i, s)| (*s, Listener::numbered_from((seq_base + i as u64 + 1) << 40))).collect();
    let mut last_second = 0;
    let mut channels: Vec<Channel> = Vec::new();
    let mut fronts: Vec<Front> = Vec::new();
    // Channels from before a retune, kept until their last decode comes back.
    let mut retired: Vec<Channel> = Vec::new();
    let mut key = None;
    // The audio decoders report here and to a status nobody reads, so they can
    // neither speak for the main receiver nor overwrite what it shows.
    let (sink, mut drained) = broadcast::channel::<SdrEvent>(4_096);
    let status = Arc::new(Mutex::new(SdrStatus::default()));
    while let Ok(block) = rx.recv() {
        if key != Some((block.rate, block.center_hz)) {
            key = Some((block.rate, block.center_hz));
            retired.append(&mut channels);
            (fronts, channels) = build(&specs, block.center_hz, block.rate);
        }
        for channel in &mut channels {
            channel.follow_main(block.main);
        }
        process_block(&mut fronts, &mut channels, &block.iq, block.ms, &sink, &status);
        for channel in channels.iter_mut().chain(retired.iter_mut()) {
            for (slot_ms, group) in channel.slots_done() {
                deliver(channel.spec, channel.center_hz, slot_ms, group, block.main, &events, &mut records);
            }
        }
        retired.retain(Channel::busy);
        CHARTING[worker].store(channels.iter().any(Channel::charting), Ordering::Relaxed);
        loop {
            match drained.try_recv() {
                Ok(SdrEvent::Decode { inspect_hz, event }) => {
                    // Only audio-driven decoders report here; a slot channel may share the dial.
                    if let Some(channel) = channels.iter().find(|c| c.spec.dial_hz == inspect_hz as u64 && matches!(c.decoder, Decoder::Hf(_))) {
                        deliver(channel.spec, channel.center_hz, block.ms, vec![event], block.main, &events, &mut records);
                    }
                }
                Ok(_) => {}
                Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
        let now = unix_now();
        if now != last_second {
            last_second = now;
            for (spec, record) in &mut records {
                let listening = block.main != Some((spec.mode, spec.dial_hz)) && channels.iter().any(|c| c.spec == *spec) && now % RECORD_S != 0;
                let radio = listening.then_some(RadioNow { dial_hz: spec.dial_hz as f64, mode: spec.mode, rfgr_db: None, cause: "wideband" });
                for segment in record.tick(now, radio, true) {
                    if let Some(log) = LOG.get() {
                        log.add_segment(segment);
                    }
                }
            }
        }
    }
}

/// The radio loop's end of the queues.
pub struct Feed {
    workers: Vec<SyncSender<Arc<Block>>>,
    specs: Vec<Spec>,
    overview: Option<crate::overview::Tap>,
}

impl Feed {
    /// The block is shared, not copied: the radio loop keeps its own
    /// reference and never writes to it again.
    pub fn feed(&self, iq: &Arc<Vec<Complex32>>, rate: f64, center_hz: f64, ms: u64, main: Option<(SdrMode, u64)>) {
        if let Some(tap) = &self.overview {
            tap.feed(iq, rate, center_hz, ms);
        }
        if !self.specs.iter().any(|s| s.heard_in(center_hz, rate)) {
            return;
        }
        let block = Arc::new(Block { iq: Arc::clone(iq), rate, center_hz, ms, main });
        for worker in &self.workers {
            if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) = worker.try_send(Arc::clone(&block)) {
                DROPPED.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Start the channels named in `USDR_WIDE`. Unset, nothing runs and the radio
/// loop does no extra work.
pub fn from_env(events: Sender<SdrEvent>) -> Option<Feed> {
    let (specs, ignored) = specs_from_env()?;
    if !ignored.is_empty() {
        eprintln!("wideband: no channel for {}", ignored.join(", "));
    }
    if specs.is_empty() {
        return None;
    }
    let mut workers = Vec::new();
    // Whole groups to a worker, so the dials that share a front end are
    // processed together.
    let grouped = groups(&specs);
    for w in 0..WORKERS {
        let (tx, rx) = sync_channel(QUEUE);
        let share: Vec<Spec> = grouped.iter().enumerate().filter(|(i, _)| i % WORKERS == w).flat_map(|(_, g)| g.iter().copied()).collect();
        let (events, base) = (events.clone(), (w * specs.len()) as u64);
        if let Err(e) = std::thread::Builder::new().name(format!("wideband-{w}")).spawn(move || run(rx, share, events, base, w)) {
            eprintln!("wideband: could not start a worker: {e}");
            return None;
        }
        workers.push(tx);
    }
    eprintln!("wideband: {} channels, {} logging", specs.len(), specs.iter().filter(|s| s.logs).count());
    Some(Feed { workers, specs, overview: crate::overview::start() })
}

/// The channels that log, by mode and dial, for the schedule to plan around.
pub fn logging_channels() -> std::collections::BTreeSet<(SdrMode, u64)> {
    specs_from_env().map(|(specs, _)| specs).unwrap_or_default().into_iter().filter(|s| s.logs).map(|s| (s.mode, s.dial_hz)).collect()
}

/// Whether a radiofax channel is in the middle of a chart, so the span must stay.
pub fn chart_busy() -> bool {
    CHARTING.iter().any(|c| c.load(Ordering::Relaxed))
}

/// What the channels have decoded, for comparing with the main decoder.
pub fn snapshot() -> Value {
    let kept = results().lock().expect("wideband results");
    json!({ "dropped_blocks": DROPPED.load(Ordering::Relaxed), "results": kept.iter().collect::<Vec<_>>() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(mode: SdrMode, dial_hz: u64) -> Spec {
        Spec::new(mode, dial_hz, true)
    }

    #[test]
    fn a_dial_needs_its_audio_inside_the_usable_span() {
        assert!(spec(SdrMode::Ft8, 3_573_000).heard_in(7_174_000.0, 8e6));
        assert!(spec(SdrMode::Ft8, 10_136_000).heard_in(7_174_000.0, 8e6));
        assert!(!spec(SdrMode::Ft8, 14_074_000).heard_in(7_174_000.0, 8e6), "the far side of the span");
        assert!(!spec(SdrMode::Ft8, 3_450_000).heard_in(7_174_000.0, 8e6));
    }

    #[test]
    fn the_channel_list_reads_as_written_and_skips_the_rest() {
        let (got, ignored) = parse_specs("FT8+7074000, WSPR?7038600,NOPE+1,FT4?not,DSC+50000000", &[]);
        assert_eq!(got, vec![Spec::new(SdrMode::Ft8, 7_074_000, true), Spec::new(SdrMode::Wspr, 7_038_600, false)]);
        assert_eq!(ignored, ["NOPE+1", "FT4?not", "DSC+50000000"], "what it cannot run, it says");
        let (got, _) = parse_specs("SSTV+14230000,SSTV?7171000,WEFAX+8502000", &[]);
        assert_eq!(got, vec![Spec::new(SdrMode::Sstv, 14_230_000, true), Spec::new(SdrMode::Sstv, 7_171_000, false), Spec::new(SdrMode::Wefax, 8_502_000, true)]);
    }

    #[test]
    fn catalog_stands_for_every_dial_the_schedule_knows() {
        let catalog = catalog_specs();
        assert!(catalog.contains(&Spec::new(SdrMode::Wefax, 12_788_000, true)), "the chart dials are in it");
        assert!(catalog.contains(&Spec::new(SdrMode::Hfdl, 15_025_000, true)));
        assert!(catalog.contains(&Spec::new(SdrMode::Sstv, 21_340_000, true)));
        // A named shadow stays a shadow, and a dial is never run twice.
        let (got, _) = parse_specs("FT8?7074000,catalog,FT8+5357000", &catalog);
        assert_eq!(got.iter().filter(|s| s.mode == SdrMode::Ft8 && s.dial_hz == 7_074_000).count(), 1);
        assert!(got.contains(&Spec::new(SdrMode::Ft8, 7_074_000, false)));
        assert!(got.contains(&Spec::new(SdrMode::Ft8, 5_357_000, true)));
        assert_eq!(got.len(), catalog.len() + 1);
        assert!(parse_specs("FT8+7074000", &catalog).0.len() == 1, "no catalog unless asked for");
    }

    #[test]
    fn sstv_takes_the_lower_sideband_below_ten_megahertz() {
        assert_eq!(spec(SdrMode::Sstv, 7_171_000).sideband(), Sideband::Lower);
        assert_eq!(spec(SdrMode::Sstv, 3_845_000).sideband(), Sideband::Lower);
        assert_eq!(spec(SdrMode::Sstv, 14_230_000).sideband(), Sideband::Upper);
        assert_eq!(spec(SdrMode::Ft8, 7_074_000).sideband(), Sideband::Upper);
        // Its audio sits below the dial, so the span has to reach down for it.
        let top = spec(SdrMode::Sstv, 7_171_000).top_hz();
        let rate = 8e6;
        // The dial just inside the span's top edge: its audio reaches down into it.
        let center = 7_171_000.0 - rate * USABLE + top / 2.0;
        assert!(spec(SdrMode::Sstv, 7_171_000).heard_in(center, rate));
        // The dial just inside the low edge: its audio falls off the bottom.
        let center = 7_171_000.0 + rate * USABLE - top / 2.0;
        assert!(!spec(SdrMode::Sstv, 7_171_000).heard_in(center, rate), "the audio is past the span's low edge");
    }

    #[test]
    fn a_beacon_channel_sits_its_pitch_below_a_beacon_frequency() {
        assert_eq!(parse_specs("BEACON+14099300", &[]).0, vec![Spec::new(SdrMode::Beacon, 14_099_300, true)]);
        let rate = 8e6;
        assert!(Channel::new(spec(SdrMode::Beacon, 14_099_300), 17_607_000.0, rate).is_some());
        assert!(Channel::new(spec(SdrMode::Beacon, 14_100_000), 17_607_000.0, rate).is_none(), "not a beacon's pitch");
        // The day slices hold all five between them.
        for f in BANDS_HZ {
            let s = spec(SdrMode::Beacon, f - BEACON_PITCH_HZ);
            assert!(s.heard_in(17_607_000.0, rate) || s.heard_in(24_620_000.0, rate), "{f}");
        }
        let h = Heard { band: 1, beacon: 13, slot_utc: 1_794_360_130, snr_db: 12.4, dashes_db: [Some(12.0), Some(2.0), None, None], steps: 2, offset_hz: -3.0, late_ms: 180, score: 40.0 };
        let ev = beacon_event(&h);
        assert_eq!(ev.summary, "OH2B · 18.110 MHz · +12 dB · down to 10 W");
        let o = observe(18_109_300.0, &ev, "", 0).unwrap();
        assert_eq!((o.mode, o.ident.as_deref(), o.grid.as_deref(), o.freq_hz, o.at_utc), ("BEACON", Some("OH2B"), Some("KP20eh"), 18_110_000, 1_794_360_130));
        assert_eq!(o.public["steps"], "2");
        assert!(!o.public.contains_key("dash3"));
    }

    /// The night spans hold 40 m's calling frequency and the day spans 20 m's.
    #[test]
    fn the_sstv_calling_frequencies_sit_in_their_spans() {
        let rate = 8e6;
        assert!(spec(SdrMode::Sstv, 7_171_000).heard_in(5_400_000.0, rate));
        assert!(spec(SdrMode::Sstv, 7_171_000).heard_in(6_855_000.0, rate));
        assert!(spec(SdrMode::Sstv, 14_230_000).heard_in(17_607_000.0, rate));
    }

    /// A tone below a lower-sideband channel's dial comes out as audio; above
    /// it, the filter takes it away. Upper sideband the other way round.
    #[test]
    fn a_channel_hears_its_own_sideband_only() {
        let rate = 250_000.0;
        let center = 7_150_000.0;
        let dial = 7_171_000_u64;
        let level = |tone_offset: f64, lower: bool| {
            let mut channel = Channel::new(Spec::new(if lower { SdrMode::Sstv } else { SdrMode::Ft8 }, dial, false), center, rate).expect("a channel");
            let (sink, _keep) = broadcast::channel(16);
            let status = Arc::new(Mutex::new(SdrStatus::default()));
            let f = dial as f64 + tone_offset - center;
            let mut energy = 0.0_f64;
            let mut n = 0_usize;
            for b in 0..20 {
                let block: Vec<Complex32> = (0..16_384)
                    .map(|i| {
                        let t = (b * 16_384 + i) as f64 / rate;
                        let p = std::f64::consts::TAU * f * t;
                        Complex32::new(0.1 * p.cos() as f32, 0.1 * p.sin() as f32)
                    })
                    .collect();
                channel.process(&block, 0, &sink, &status);
                if b >= 10 {
                    energy += channel.linear.iter().map(|v| f64::from(*v) * f64::from(*v)).sum::<f64>();
                    n += channel.linear.len();
                }
            }
            energy / n.max(1) as f64
        };
        let (lsb_wanted, lsb_unwanted) = (level(-1_900.0, true), level(1_900.0, true));
        assert!(lsb_wanted > 100.0 * lsb_unwanted, "LSB: {lsb_wanted} against {lsb_unwanted}");
        let (usb_wanted, usb_unwanted) = (level(1_900.0, false), level(-1_900.0, false));
        assert!(usb_wanted > 100.0 * usb_unwanted, "USB: {usb_wanted} against {usb_unwanted}");
    }

    #[test]
    fn dials_close_together_share_a_front_end() {
        let specs = [spec(SdrMode::Ft8, 7_074_000), spec(SdrMode::Wspr, 7_038_600), spec(SdrMode::Dsc, 8_412_800), spec(SdrMode::Ft8, 10_136_000), spec(SdrMode::Wspr, 10_138_700)];
        let (fronts, channels) = build(&specs, 6_855_000.0, 8e6);
        assert_eq!(fronts.len(), 2);
        assert_eq!(channels.len(), 5);
        let front_of = |dial: u64| channels.iter().find(|c| c.spec.dial_hz == dial).and_then(|c| c.front);
        assert!(front_of(7_074_000).is_some() && front_of(7_074_000) == front_of(7_038_600));
        assert!(front_of(10_136_000).is_some() && front_of(10_136_000) == front_of(10_138_700));
        assert_eq!(front_of(8_412_800), None);
        // The decoders see the rate they always did.
        let alone = Channel::new(spec(SdrMode::Ft8, 7_074_000), 6_855_000.0, 8e6).expect("a channel");
        assert!(channels.iter().all(|c| c.chain.fs_out() == alone.chain.fs_out()));
    }

    #[test]
    fn a_signal_in_a_shared_front_end_is_decoded() {
        let rate = 2_000_000.0;
        let center = 7_000_000.0;
        let dial = 7_074_000_u64;
        let offset = dial as f64 - center;
        let start_ms = 1_800_000_000_000_u64;
        let audio = scannerd_engine::ft8::synthesize(scannerd_engine::ft8::Protocol::Ft8, "CQ K1ABC FN42", 1_500.0, 0.5, 15.0).expect("a message");
        let (mut fronts, mut channels) = build(&[spec(SdrMode::Ft8, dial), spec(SdrMode::Wspr, 7_038_600)], center, rate);
        assert_eq!(fronts.len(), 1);
        let (sink, _keep) = broadcast::channel(16);
        let status = Arc::new(Mutex::new(SdrStatus::default()));
        let total = (rate * 16.0) as usize;
        let mut sent = 0;
        let mut block = Vec::with_capacity(26_624);
        while sent < total {
            block.clear();
            for n in sent..(sent + 26_624).min(total) {
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
            process_block(&mut fronts, &mut channels, &block, start_ms + (sent as f64 / rate * 1_000.0) as u64, &sink, &status);
        }
        let ft8 = channels.iter_mut().find(|c| c.spec.dial_hz == dial).expect("the FT8 channel");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        let mut got = Vec::new();
        while got.is_empty() && std::time::Instant::now() < deadline {
            got = ft8.slots_done();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(got.iter().flat_map(|(_, g)| g).any(|e| e.summary.contains("K1ABC")), "{got:?}");
    }

    /// A chart through a channel: start tone, phasing, picture, stop tone.
    /// The channel says it is charting from the start to the stop, and the
    /// picture comes out as the main receiver's would.
    #[test]
    fn a_radiofax_channel_receives_a_chart_on_its_own_start_tone() {
        let rate = 250_000.0;
        let center = 9_050_000.0;
        let dial = 9_108_100_u64;
        let mut channel = Channel::new(spec(SdrMode::Wefax, dial), center, rate).expect("a channel");
        let (sink, mut rx) = broadcast::channel(8_192);
        let status = Arc::new(Mutex::new(SdrStatus::default()));
        // The audio frequency at time t: 1500 Hz black, 2300 Hz white, 120 LPM.
        let line_s = 0.5;
        let square = |t: f64, hz: f64| if (t * hz).fract() < 0.5 { 1_500.0 } else { 2_300.0 };
        let audio_hz = |t: f64| -> f64 {
            if t < 5.0 {
                square(t, 300.0)
            } else if t < 15.0 {
                // Phasing: white, with a black pulse 5% of the line wide.
                if ((t - 5.0) / line_s).fract() < 0.05 { 1_500.0 } else { 2_300.0 }
            } else if t < 25.0 {
                // A gray ramp across each line.
                1_500.0 + 800.0 * ((t - 15.0) / line_s).fract()
            } else if t < 30.0 {
                square(t, 450.0)
            } else {
                1_900.0
            }
        };
        let offset = dial as f64 - center;
        let mut phase = 0.0_f64;
        // Noise about 25 dB down in 3 kHz: a carrier with none at all makes
        // false start tones out of rounding.
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut noise = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            ((seed >> 11) as f64 / (1_u64 << 53) as f64 - 0.5) * 0.35
        };
        let mut seen_charting = false;
        let mut events = Vec::new();
        let block = 25_000;
        for b in 0..(33.0 * rate / block as f64) as usize {
            let iq: Vec<Complex32> = (0..block)
                .map(|i| {
                    let t = (b * block + i) as f64 / rate;
                    phase = (phase + std::f64::consts::TAU * (offset + audio_hz(t)) / rate) % std::f64::consts::TAU;
                    Complex32::new((0.3 * phase.cos() + noise()) as f32, (0.3 * phase.sin() + noise()) as f32)
                })
                .collect();
            channel.process(&iq, (b * block) as u64 * 1_000 / rate as u64, &sink, &status);
            seen_charting |= channel.charting();
            while let Ok(ev) = rx.try_recv() {
                if let SdrEvent::Decode { event, .. } = ev {
                    events.push(event);
                }
            }
        }
        let kinds: Vec<&str> = events.iter().filter(|e| e.protocol == "WEFAX").map(|e| e.kind.as_str()).collect();
        assert!(seen_charting, "{kinds:?}");
        assert!(!channel.charting(), "the stop tone ends it: {kinds:?}");
        assert_eq!(kinds, vec!["start", "picture"]);
        let lines: u32 = events.iter().find(|e| e.kind == "picture").and_then(|e| e.fields.get("lines")?.parse().ok()).unwrap_or(0);
        assert!(lines >= 10, "{lines} lines");
    }

    #[test]
    fn a_signal_off_center_in_the_span_is_decoded() {
        let rate = 250_000.0;
        let offset = 30_000.0;
        let center = 7_044_000.0;
        let dial = 7_074_000_u64;
        let start_ms = 1_800_000_000_000_u64;
        let audio = scannerd_engine::ft8::synthesize(scannerd_engine::ft8::Protocol::Ft8, "CQ K1ABC FN42", 1_500.0, 0.5, 15.0).expect("a message");
        let mut channel = Channel::new(spec(SdrMode::Ft8, dial), center, rate).expect("a channel");
        let (sink, _keep) = broadcast::channel(16);
        let status = Arc::new(Mutex::new(SdrStatus::default()));
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
            channel.process(&block, start_ms + (sent as f64 / rate * 1_000.0) as u64, &sink, &status);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        let mut got = Vec::new();
        while got.is_empty() && std::time::Instant::now() < deadline {
            got = channel.slots_done();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(got.iter().flat_map(|(_, g)| g).any(|e| e.summary.contains("K1ABC")), "{got:?}");
    }
}
