//! Real-time Digital Traffic Signal Classifier and Inspector.
//!
//! Analyzes baseband complex IQ and frequency discriminator streams to identify
//! modulation types (2-FSK/GMSK, 4-FSK, AFSK, analog FM, CW) and specific protocols:
//! - P25 Phase 1, including NAC and DUID extraction;
//! - DMR Tier II/Tier III, Connect Plus, Capacity Plus, and Hytera XPT;
//! - NXDN48/NXDN96 conventional and Type-C/Type-D control signalling;
//! - Motorola SmartNet/SmartZone, LTR Standard, and Passport;
//! - POCSAG and FLEX paging;
//! - APRS/AX.25 Bell 202 and NOAA SAME;
//! - analog FM with CTCSS/DCS, CW, and unmodulated carriers.
//!
//! Exact frame sync always wins.  When a complete sync word is not present, a
//! clock/level analysis reports a modulation family and a short list of honest
//! protocol candidates instead of guessing one protocol from deviation alone.

use crate::ctcss::CtcssDetector;
use crate::dcs::DcsDetector;
use crate::dmr::{
    BurstKind, CsbkError, DataAssembler, DataError, DataMessage, DataPdu, DmrReceiver,
    EmbeddedLcAssembler, SyncSource, decode_csbk, decode_data_pdu,
};
use crate::flex::FlexDecoder;
use crate::legacy_digital::{LegacyDigitalDetector, LegacyFrame};
use crate::ltr::LtrDecoder;
use crate::nbfm::{Demodulated, NbfmDemod};
use crate::nxdn::{NxdnReceiver, Rate as NxdnRate};
use crate::p25::{C4fmFrontEnd, Duid, FrameDetector, decode_pdu};
use crate::passport::PassportDecoder;
use crate::pocsag::PocsagDecoder;
use crate::smartnet::SmartNetDecoder;
use num_complex::Complex32;
use scannerd_dsp::DecimFir;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::f32::consts::PI;

/// Result of digital traffic signal classification.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ClassificationResult {
    pub active: bool,
    pub snr_db: f32,
    pub rf_dbfs: f32,
    pub peak_dev_hz: f32,
    pub rms_dev_hz: f32,
    pub center_offset_hz: f32,
    pub modulation: String,
    pub protocol: String,
    pub details: Option<String>,
    pub confidence: f32,
}

impl Default for ClassificationResult {
    fn default() -> Self {
        Self {
            active: false,
            snr_db: 0.0,
            rf_dbfs: -100.0,
            peak_dev_hz: 0.0,
            rms_dev_hz: 0.0,
            center_offset_hz: 0.0,
            modulation: "None".into(),
            protocol: "Idle / Noise".into(),
            details: None,
            confidence: 0.0,
        }
    }
}

/// A decoder observation intended for the SDR protocol inspector. Unlike a
/// classification label, this represents an actual frame/burst attempt.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DecodeEvent {
    pub protocol: String,
    pub kind: String,
    pub summary: String,
    pub valid: bool,
    pub fields: BTreeMap<String, String>,
}

/// Per-protocol channel filters.
///
/// The inspector hands every matcher the same 25 kHz-wide baseband, because
/// that is the widest thing it has to carry. That is the wrong filter for
/// most of what it then tries to decode: NXDN48 occupies about 5 kHz, and
/// giving its receiver five times that bandwidth admits five times the noise
/// plus whatever sits in the adjacent channels. op25 and dsd give each
/// protocol a filter matched to its own occupied bandwidth, and the
/// difference is several dB of sensitivity on exactly the weak signals worth
/// classifying.
///
/// These run at decimation 1 — the rate is already the inspect rate; only the
/// bandwidth changes.
struct ChannelFilters {
    /// ~5 kHz for NXDN48 (2400 sym/s, 1.05 kHz deviation).
    narrow: DecimFir,
    /// ~12.5 kHz for P25 Phase 1, DMR and NXDN96.
    standard: DecimFir,
    narrow_buf: Vec<Complex32>,
    standard_buf: Vec<Complex32>,
}

impl ChannelFilters {
    /// Half-bandwidths, in Hz.
    const NARROW_HZ: f32 = 2_500.0;
    const STANDARD_HZ: f32 = 6_250.0;
    const TAPS: usize = 129;

    fn new(fs: f64) -> Self {
        let fs = fs as f32;
        Self {
            narrow: DecimFir::new(Self::NARROW_HZ.min(fs * 0.45), fs, 1, Self::TAPS),
            standard: DecimFir::new(Self::STANDARD_HZ.min(fs * 0.45), fs, 1, Self::TAPS),
            narrow_buf: Vec::new(),
            standard_buf: Vec::new(),
        }
    }

    fn process(&mut self, iq: &[Complex32]) {
        self.narrow.process(iq, &mut self.narrow_buf);
        self.standard.process(iq, &mut self.standard_buf);
    }
}

pub struct SignalClassifier {
    fs: f64,
    nbfm: NbfmDemod,
    disc_nbfm: NbfmDemod,
    ctcss: CtcssDetector,
    dcs: DcsDetector,
    demod: Demodulated,
    disc_buf: Vec<f32>,
    dev_abs_buf: Vec<f32>,
    filters: ChannelFilters,
    p25_frontend: C4fmFrontEnd,
    p25_detector: FrameDetector,
    p25_buf: Vec<f32>,
    dmr_receiver: DmrReceiver,
    /// Holds a DMR data header open until its blocks arrive, so the payload
    /// can be shown rather than only the envelope.
    dmr_data: [DataAssembler; 3],
    dmr_embedded_lc: [EmbeddedLcAssembler; 3],
    nxdn48_receiver: NxdnReceiver,
    nxdn96_receiver: NxdnReceiver,
    smartnet_decoder: SmartNetDecoder,
    pocsag_decoder: PocsagDecoder,
    flex_decoder: FlexDecoder,
    ltr_decoder: LtrDecoder,
    passport_decoder: PassportDecoder,
    legacy_digital: LegacyDigitalDetector,
    decode_events: Vec<DecodeEvent>,
    last_decode_events: BTreeMap<String, std::time::Instant>,
    /// Rolling discriminator history. RTL-SDR blocks are commonly shorter than
    /// one P25/DMR sync word after channel decimation, so frame correlation must
    /// span input blocks.
    analysis_buf: Vec<f32>,
    samples_since_analysis: usize,
    last_clocked: Option<ClockedFeatures>,
    noise_floor_dbfs: f32,
    noise_floor_initialized: bool,
    /// Startup collection: running minimum and block count, so the floor is
    /// seeded from the quietest of the first second rather than whatever was
    /// on the air when the receiver opened.
    init_min: f32,
    init_blocks: u8,
    quiet_blocks: u8,
    last_p25_match: Option<(u16, u8, f32, usize, std::time::Instant)>,
    /// Strong P25 frame syncs whose NID was too damaged to BCH-correct. Two
    /// nearby syncs are still better protocol evidence than one coincidental
    /// DMR-shaped centre field, and let a marginal P25 carrier retain
    /// ownership while the full decoder works toward a valid NAC.
    last_p25_candidate: Option<(f32, u8, std::time::Instant)>,
    last_dmr_match: Option<(
        &'static str,
        &'static str,
        Option<u8>,
        Option<u8>,
        f32,
        std::time::Instant,
    )>,
    last_nxdn_match: Option<(NxdnRate, String, u8, &'static str, f32, std::time::Instant)>,
    last_smartnet_match: Option<(u16, String, u8, usize, usize, std::time::Instant)>,
    last_pocsag_match: Option<(u32, u32, std::time::Instant)>,
    last_flex_match: Option<(u32, u64, std::time::Instant)>,
    last_ltr_match: Option<(u8, u8, u8, u8, u8, std::time::Instant)>,
    last_passport_match: Option<(u8, u16, u8, u16, String, std::time::Instant)>,
    last_legacy_match: Option<(LegacyFrame, std::time::Instant)>,
    last_aprs_match: Option<std::time::Instant>,
    last_same_match: Option<std::time::Instant>,
    last_ctcss_match: Option<(f32, std::time::Instant)>,
    last_dcs_match: Option<(u16, std::time::Instant)>,
    /// Pager banks (POCSAG + FLEX) are owned and run by the caller in
    /// PACKET/AUTO mode — the same discriminator would otherwise be sliced by
    /// two full sets of timing lanes. The classifier still reports protocol
    /// matches, fed back through [`SignalClassifier::note_pocsag_sync`] /
    /// [`note_flex_sync`].
    pagers_external: bool,
    /// Messages recovered by the internal banks this block, drained by the
    /// event pass later in `process`. Held as fields to keep the borrow of
    /// `self.disc_buf` and the decoders inside one method.
    pending_pocsag_messages: Vec<crate::pocsag::PocsagMessage>,
    pending_flex_messages: Vec<crate::flex::FlexMessage>,
}

impl SignalClassifier {
    pub fn new(fs: f64) -> Self {
        Self {
            fs,
            nbfm: NbfmDemod::new(fs),
            disc_nbfm: NbfmDemod::new(fs),
            ctcss: CtcssDetector::new(fs),
            dcs: DcsDetector::new(fs),
            demod: Demodulated::default(),
            disc_buf: Vec::with_capacity(4096),
            dev_abs_buf: Vec::with_capacity(4096),
            filters: ChannelFilters::new(fs),
            p25_frontend: C4fmFrontEnd::new(fs),
            p25_detector: FrameDetector::with_packet_payload(),
            p25_buf: Vec::with_capacity(4096),
            dmr_receiver: DmrReceiver::new(fs),
            dmr_data: std::array::from_fn(|_| DataAssembler::new()),
            dmr_embedded_lc: std::array::from_fn(|_| EmbeddedLcAssembler::new()),
            nxdn48_receiver: NxdnReceiver::new(NxdnRate::Nxdn48, fs),
            nxdn96_receiver: NxdnReceiver::new(NxdnRate::Nxdn96, fs),
            smartnet_decoder: SmartNetDecoder::new(fs),
            pocsag_decoder: PocsagDecoder::auto(fs),
            flex_decoder: FlexDecoder::new(fs),
            ltr_decoder: LtrDecoder::new(fs),
            passport_decoder: PassportDecoder::new(fs),
            legacy_digital: LegacyDigitalDetector::new(fs),
            decode_events: Vec::new(),
            last_decode_events: BTreeMap::new(),
            analysis_buf: Vec::with_capacity(24_000),
            samples_since_analysis: 0,
            last_clocked: None,
            noise_floor_dbfs: -95.0,
            noise_floor_initialized: false,
            init_min: f32::INFINITY,
            init_blocks: 0,
            quiet_blocks: 0,
            last_p25_match: None,
            last_p25_candidate: None,
            last_dmr_match: None,
            last_nxdn_match: None,
            last_smartnet_match: None,
            last_pocsag_match: None,
            last_flex_match: None,
            last_ltr_match: None,
            last_passport_match: None,
            last_legacy_match: None,
            last_aprs_match: None,
            last_same_match: None,
            last_ctcss_match: None,
            last_dcs_match: None,
            pagers_external: false,
            pending_pocsag_messages: Vec::new(),
            pending_flex_messages: Vec::new(),
        }
    }

    /// Hand the POCSAG and FLEX banks to the caller.
    ///
    /// In PACKET and AUTO mode the server already runs a full set of pager
    /// decoders over the same discriminator; letting the classifier run its
    /// own second set doubled the per-sample cost of the largest always-on
    /// decoders for no extra frames. The classifier's protocol matching still
    /// works — the caller feeds sync observations back through
    /// [`Self::note_pocsag_sync`] / [`Self::note_flex_sync`] — but its internal
    /// lanes are dropped.
    pub fn use_external_pagers(&mut self) {
        if !self.pagers_external {
            self.pagers_external = true;
            self.pocsag_decoder = PocsagDecoder::idle();
            self.flex_decoder = FlexDecoder::idle();
        }
    }

    pub fn reset(&mut self) {
        self.nbfm.reset();
        self.disc_nbfm.reset();
        self.ctcss.reset();
        self.dcs.reset();
        self.demod.clear();
        self.disc_buf.clear();
        self.dev_abs_buf.clear();
        self.filters = ChannelFilters::new(self.fs);
        self.p25_frontend = C4fmFrontEnd::new(self.fs);
        self.p25_detector = FrameDetector::with_packet_payload();
        self.p25_buf.clear();
        self.dmr_receiver = DmrReceiver::new(self.fs);
        for assembler in &mut self.dmr_data {
            assembler.reset();
        }
        for assembler in &mut self.dmr_embedded_lc {
            assembler.clear();
        }
        self.nxdn48_receiver = NxdnReceiver::new(NxdnRate::Nxdn48, self.fs);
        self.nxdn96_receiver = NxdnReceiver::new(NxdnRate::Nxdn96, self.fs);
        self.smartnet_decoder.reset();
        if !self.pagers_external {
            self.pocsag_decoder = PocsagDecoder::auto(self.fs);
            self.flex_decoder = FlexDecoder::new(self.fs);
        } else {
            self.pocsag_decoder = PocsagDecoder::idle();
            self.flex_decoder = FlexDecoder::idle();
        }
        self.ltr_decoder.reset();
        self.passport_decoder.reset();
        self.legacy_digital.reset();
        self.decode_events.clear();
        self.last_decode_events.clear();
        self.analysis_buf.clear();
        self.samples_since_analysis = 0;
        self.last_clocked = None;
        self.noise_floor_initialized = false;
        self.init_min = f32::INFINITY;
        self.init_blocks = 0;
        self.quiet_blocks = 0;
        self.last_p25_match = None;
        self.last_p25_candidate = None;
        self.last_dmr_match = None;
        self.last_nxdn_match = None;
        self.last_smartnet_match = None;
        self.last_pocsag_match = None;
        self.last_flex_match = None;
        self.last_ltr_match = None;
        self.last_passport_match = None;
        self.last_legacy_match = None;
        self.last_aprs_match = None;
        self.last_same_match = None;
        self.last_ctcss_match = None;
        self.last_dcs_match = None;
    }

    /// Drain frame-level observations accumulated by the most recent calls.
    pub fn take_decode_events(&mut self) -> Vec<DecodeEvent> {
        std::mem::take(&mut self.decode_events)
    }

    /// De-emphasized, voice-band NBFM from the last processed IQ block.
    pub fn monitor_voice(&self) -> &[f32] {
        &self.demod.voice
    }

    /// High-passed discriminator envelope from the last processed IQ block:
    /// the noise reference the audio gate keys on.
    pub fn monitor_noise(&self) -> &[f32] {
        &self.demod.noise
    }

    /// FM discriminator in hertz from the last processed IQ block.
    pub fn monitor_discriminator_hz(&self) -> &[f32] {
        &self.disc_buf
    }

    /// Record a POCSAG sync observed in an externally-owned decoder bank.
    pub fn note_pocsag_sync(&mut self, baud: u32) {
        self.pocsag_decoder.note_sync(baud);
    }

    /// Record a FLEX sync observed in an externally-owned decoder bank.
    pub fn note_flex_sync(&mut self, baud: u32) {
        self.flex_decoder.note_sync(baud);
    }

    fn publish_decode(&mut self, event: DecodeEvent, now: std::time::Instant) {
        let key = format!("{}:{}:{}", event.protocol, event.kind, event.summary);
        if self
            .last_decode_events
            .get(&key)
            .is_some_and(|at| now.duration_since(*at) < std::time::Duration::from_millis(350))
        {
            return;
        }
        self.last_decode_events.insert(key, now);
        if self.last_decode_events.len() > 128 {
            self.last_decode_events
                .retain(|_, at| now.duration_since(*at) < std::time::Duration::from_secs(10));
        }
        if self.decode_events.len() >= 64 {
            self.decode_events.remove(0);
        }
        self.decode_events.push(event);
    }

    /// Process a block of baseband IQ samples and return the current classification.
    pub fn process(&mut self, iq: &[Complex32], tuned_freq_hz: f64) -> ClassificationResult {
        if iq.is_empty() {
            return ClassificationResult::default();
        }

        // 1. Measure Signal Power & SNR
        let pwr_sum: f32 = iq.iter().map(|c| c.norm_sqr()).sum();
        let pwr_mean = pwr_sum / iq.len() as f32;
        let rf_dbfs = 10.0 * (pwr_mean + 1e-12).log10();

        // Track the noise reference with minimum-statistics semantics: fall
        // onto lower readings promptly, rise under higher ones very slowly.
        // The old single-rate EMA let a persistent carrier drag the floor up
        // to within 3 dB of itself in ~10 minutes, collapsing its own SNR
        // until every downstream gate treated it as noise. Startup takes the
        // running MINIMUM of the first second so a burst at boot cannot pin
        // the floor high.
        if !self.noise_floor_initialized {
            self.init_min = self.init_min.min(rf_dbfs);
            self.init_blocks += 1;
            if self.init_blocks >= 25 {
                self.noise_floor_dbfs = self.init_min - 3.0;
                self.noise_floor_initialized = true;
            } else {
                // Provisional: well below anything seen, so real traffic is
                // not suppressed while the reference is still collecting.
                self.noise_floor_dbfs = self.init_min - 6.0;
            }
        } else {
            let a = if rf_dbfs < self.noise_floor_dbfs {
                0.25
            } else {
                0.002
            };
            self.noise_floor_dbfs += a * (rf_dbfs - self.noise_floor_dbfs);
        }
        let snr_db = (rf_dbfs - self.noise_floor_dbfs).max(0.0);

        // 2. Demodulate & Measure Deviation
        self.disc_buf.clear();
        // Keep discriminator and audio state independent. Reprocessing a block
        // through one delay-line demodulator made the audio path's first sample
        // compare the end of the block with its beginning on every call.
        self.disc_nbfm.discriminator_hz(iq, &mut self.disc_buf);

        self.analysis_buf.extend_from_slice(&self.disc_buf);
        self.samples_since_analysis = self
            .samples_since_analysis
            .saturating_add(self.disc_buf.len());
        // Half a second catches slow pager sync while keeping the per-update
        // correlators bounded. Preserve the newest samples at block boundaries.
        let history_len = (self.fs * 0.5).round().max(4096.0) as usize;
        if self.analysis_buf.len() > history_len {
            let excess = self.analysis_buf.len() - history_len;
            self.analysis_buf.drain(..excess);
        }

        let pocsag_hit = if self.pagers_external {
            // The caller owns the lanes; it reports syncs through
            // note_pocsag_sync. Nothing to run here.
            None
        } else {
            let syncs_before = {
                let d = self.pocsag_decoder.diagnostics();
                d.syncs_512 + d.syncs_1200 + d.syncs_2400
            };
            let pocsag_messages = self.pocsag_decoder.process(&self.disc_buf);
            let hit = {
                let d = self.pocsag_decoder.diagnostics();
                let syncs_after = d.syncs_512 + d.syncs_1200 + d.syncs_2400;
                (syncs_after > syncs_before).then(|| {
                    (
                        d.last_sync_baud.unwrap_or(0),
                        pocsag_messages.first().map(|m| m.capcode).unwrap_or(0),
                    )
                })
            };
            self.pending_pocsag_messages = pocsag_messages;
            hit
        };
        let flex_hit = if self.pagers_external {
            None
        } else {
            let flex_syncs_before = {
                let d = self.flex_decoder.diagnostics();
                (d.syncs_1600, d.syncs_3200, d.syncs_6400)
            };
            let flex_messages = self.flex_decoder.process(&self.disc_buf);
            let hit = {
                let d = self.flex_decoder.diagnostics();
                let baud = if d.syncs_6400 > flex_syncs_before.2 {
                    Some(6400)
                } else if d.syncs_3200 > flex_syncs_before.1 {
                    Some(3200)
                } else if d.syncs_1600 > flex_syncs_before.0 {
                    Some(1600)
                } else {
                    None
                };
                baud.map(|baud| (baud, flex_messages.first().map(|m| m.capcode).unwrap_or(0)))
            };
            self.pending_flex_messages = flex_messages;
            hit
        };

        self.demod.clear();
        self.nbfm.process(iq, &mut self.demod);

        // Each receiver gets the bandwidth its protocol actually occupies
        // rather than the inspector's widest one. Taking the buffers out of
        // `self` keeps the borrow checker happy while the receivers, which
        // are also `self`, run over them.
        self.filters.process(iq);
        let narrow = std::mem::take(&mut self.filters.narrow_buf);
        let standard = std::mem::take(&mut self.filters.standard_buf);

        // Reuse the production P25 front end and BCH-validating frame detector.
        // The former classifier had a second, simplified slicer which disagreed
        // with the actual P25 decoder and could not span short SDR blocks.
        self.p25_buf.clear();
        self.p25_frontend.process(&standard, &mut self.p25_buf);
        let p25_frames = self.p25_detector.push(&self.p25_buf);
        let dmr_bursts = self.dmr_receiver.process(&standard);
        let mut nxdn_frames = self.nxdn48_receiver.process(&narrow);
        nxdn_frames.extend(self.nxdn96_receiver.process(&standard));
        self.filters.narrow_buf = narrow;
        self.filters.standard_buf = standard;
        let smartnet_osws = self.smartnet_decoder.process(&self.disc_buf);
        // Three decibels is below anything that could actually sync, so this
        // only skips channels with nothing on them — where the sweep used to
        // spend a quarter of the classifier's time finding patterns in noise.
        // Zero dB is the honest floor of a min-statistics reference: below it
        // there is provably nothing above noise, so skipping saves the sweep
        // from fitting names to noise. Above it, weak-but-real traffic gets
        // its chance — the cadence qualification inside rejects false syncs.
        let legacy_frames = self.legacy_digital.process(&self.disc_buf, snr_db > 0.0);

        // One walk accumulates Σx and Σx²; mean and rms both follow, and the
        // |x − mean| fill reuses nothing extra. (This used to be three
        // separate passes plus the abs-fill.)
        let (mut sum, mut sumsq) = (0.0f64, 0.0f64);
        for &x in &self.disc_buf {
            sum += f64::from(x);
            sumsq += f64::from(x) * f64::from(x);
        }
        let n = self.disc_buf.len().max(1) as f64;
        let mean_offset_hz = (sum / n) as f32;
        self.dev_abs_buf.clear();
        self.dev_abs_buf
            .extend(self.disc_buf.iter().map(|&x| (x - mean_offset_hz).abs()));
        // A low-amplitude IQ sample has an undefined phase and can create a
        // one-sample Nyquist spike. Report the 99th percentile as peak
        // deviation so one such sample cannot mark a clean carrier as noise.
        // Only that one order statistic is read, so an O(n) partition
        // replaces the full O(n log n) sort this used to pay per block.
        let peak_idx = ((self.dev_abs_buf.len() as f32 * 0.99) as usize)
            .min(self.dev_abs_buf.len().saturating_sub(1));
        let peak_dev_hz = if self.dev_abs_buf.is_empty() {
            0.0
        } else {
            let (idx, buf) = (peak_idx, &mut self.dev_abs_buf);
            buf.select_nth_unstable_by(idx, f32::total_cmp);
            buf[idx]
        };
        let rms_dev_hz = ((sumsq - sum * sum / n).max(0.0) / n).sqrt() as f32;

        // An FM carrier quiets the discriminator. Filtered receiver noise can
        // have plenty of RF power but spans most of the 48 kHz channel and must
        // never be offered to protocol matchers as traffic.
        //
        // Both gates are RELATIVE, not absolute. The fixed 5 kHz rms bound
        // marked genuinely weak carriers inactive — at low SNR the
        // discriminator is noise-dominated and routinely exceeds it — and
        // -85 dBFS depends entirely on where the operator set the RF gain.
        // What identifies a carrier at any level is quieting: its deviation
        // spread collapses toward its tone amplitude while pure noise stays
        // wide. `snr_db` against a min-statistics floor measures exactly that.
        let discriminator_quiet = peak_dev_hz < 14_000.0
            && (rms_dev_hz < 5_000.0 || (snr_db >= 3.0 && rms_dev_hz < 8_000.0));
        // Absolute backstop for digital silence (ADC zero): no relative floor
        // can reject true nothing.
        let signal_present =
            rf_dbfs > self.noise_floor_dbfs + 2.0 && rf_dbfs > -110.0;
        if discriminator_quiet && signal_present {
            self.quiet_blocks = self.quiet_blocks.saturating_add(1).min(8);
        } else {
            self.quiet_blocks = 0;
        }
        let active = signal_present
            && discriminator_quiet
            && (self.quiet_blocks >= 2 || rf_dbfs > -25.0 || snr_db >= 5.0);

        let now = std::time::Instant::now();
        let hold_duration = std::time::Duration::from_millis(2500);

        for frame in legacy_frames {
            let mut fields = BTreeMap::new();
            fields.insert("baud".into(), frame.baud.to_string());
            fields.insert("modulation".into(), frame.modulation.into());
            fields.insert(
                "polarity".into(),
                if frame.inverted { "inverted" } else { "normal" }.into(),
            );
            fields.insert("syncErrors".into(), frame.sync_errors.to_string());
            fields.insert("cadenceHits".into(), frame.cadence_hits.to_string());
            self.publish_decode(
                DecodeEvent {
                    protocol: frame.protocol.into(),
                    kind: frame.kind.into(),
                    summary: format!(
                        "{} · {} Bd{}",
                        frame.kind,
                        frame.baud,
                        if frame.inverted { " · inverted" } else { "" }
                    ),
                    valid: true,
                    fields,
                },
                now,
            );
            self.last_legacy_match = Some((frame, now));
        }

        for frame in p25_frames {
            if frame.correlation >= 0.72 && (500.0..=3_500.0).contains(&frame.deviation_hz) {
                let hits = self
                    .last_p25_candidate
                    .filter(|(_, _, at)| {
                        now.duration_since(*at) < std::time::Duration::from_millis(900)
                    })
                    .map(|(_, hits, _)| hits.saturating_add(1))
                    .unwrap_or(1)
                    .min(8);
                self.last_p25_candidate = Some((frame.correlation, hits, now));
            }
            if frame.bch_ok && frame.duid.is_known() {
                let duid = match frame.duid {
                    Duid::Header => 0x0,
                    Duid::Terminator => 0x3,
                    Duid::Ldu1 => 0x5,
                    Duid::Tsdu => 0x7,
                    Duid::Ldu2 => 0xA,
                    Duid::Pdu => 0xC,
                    Duid::TerminatorLc => 0xF,
                    Duid::Unknown(v) => v,
                };
                self.last_p25_match = Some((
                    frame.nac,
                    duid,
                    frame.correlation,
                    frame.corrected_bits,
                    now,
                ));
                let mut fields = BTreeMap::new();
                fields.insert("nac".into(), format!("${:03X}", frame.nac));
                fields.insert("duid".into(), format!("{:?}", frame.duid));
                fields.insert("sync".into(), format!("{:.0}%", frame.correlation * 100.0));
                fields.insert("bchCorrections".into(), frame.corrected_bits.to_string());
                self.publish_decode(
                    DecodeEvent {
                        protocol: "P25 Phase 1".into(),
                        kind: "frame".into(),
                        valid: true,
                        summary: format!("NAC ${:03X} · {:?}", frame.nac, frame.duid),
                        fields,
                    },
                    now,
                );
                if frame.duid == Duid::Pdu
                    && let Some(message) = decode_pdu(&frame.payload)
                {
                    let header = &message.header;
                    let mut fields = BTreeMap::new();
                    fields.insert("format".into(), format!("0x{:02X}", header.format));
                    fields.insert("formatName".into(), header.format_name.clone());
                    fields.insert(
                        "sap".into(),
                        format!("0x{:02X}", header.service_access_point),
                    );
                    fields.insert("service".into(), header.service_name.clone());
                    fields.insert("llid".into(), header.logical_link_id.to_string());
                    fields.insert("blocks".into(), header.blocks_to_follow.to_string());
                    fields.insert(
                        "manufacturerId".into(),
                        format!("0x{:02X}", header.manufacturer_id),
                    );
                    fields.insert("complete".into(), message.complete.to_string());
                    if let Some(crc_ok) = message.crc_ok {
                        fields.insert("crc32".into(), crc_ok.to_string());
                    }
                    if !message.payload_hex.is_empty() {
                        fields.insert("payload".into(), message.payload_hex.clone());
                    }
                    if let Some(application) = &message.application {
                        fields.insert("application".into(), application.clone());
                    }
                    if let Some(source) = &message.source_ip {
                        fields.insert("sourceIp".into(), source.clone());
                    }
                    if let Some(destination) = &message.destination_ip {
                        fields.insert("destinationIp".into(), destination.clone());
                    }
                    if let Some(port) = message.source_port {
                        fields.insert("sourcePort".into(), port.to_string());
                    }
                    if let Some(port) = message.destination_port {
                        fields.insert("destinationPort".into(), port.to_string());
                    }
                    if !message.vendor_blocks.is_empty() {
                        fields.insert("vendorBlocks".into(), message.vendor_blocks.join(","));
                    }
                    self.publish_decode(
                        DecodeEvent {
                            protocol: "P25 Phase 1".into(),
                            kind: if message.complete {
                                "packet data".into()
                            } else {
                                "packet data header".into()
                            },
                            summary: format!(
                                "{} · {} · LLID {} · {} blocks",
                                header.format_name,
                                header.service_name,
                                header.logical_link_id,
                                header.blocks_to_follow
                            ),
                            valid: true,
                            fields,
                        },
                        now,
                    );
                }
            }
        }
        for burst in dmr_bursts {
            // A DMR sync word alone is not enough in a general-purpose
            // classifier: an isolated P25 payload collision can meet the
            // four-dibit acquisition budget. Real DMR immediately supplies a
            // Golay-protected Slot Type (data) or QR-protected EMB (voice),
            // both of which carry the color code and corroborate the sync.
            let Some(color_code) = burst.color_code() else {
                continue;
            };
            let source = match burst.source {
                SyncSource::Bs => "Base station",
                SyncSource::Ms => "Mobile",
                SyncSource::Unknown => "DMR",
            };
            let kind = match &burst.kind {
                BurstKind::Voice { .. } => "voice",
                BurstKind::Data { .. } => "data",
            };
            self.last_dmr_match = Some((
                source,
                kind,
                Some(color_code),
                burst.slot,
                self.dmr_receiver.quality_db(),
                now,
            ));
            let mut burst_fields = BTreeMap::new();
            if let Some(v) = burst.slot {
                burst_fields.insert("slot".into(), v.to_string());
            }
            burst_fields.insert("colorCode".into(), color_code.to_string());
            burst_fields.insert("source".into(), source.into());
            burst_fields.insert(
                "qualityDb".into(),
                format!("{:.1}", self.dmr_receiver.quality_db()),
            );
            self.publish_decode(
                DecodeEvent {
                    protocol: "DMR".into(),
                    kind: "burst".into(),
                    valid: true,
                    summary: format!(
                        "{source} {kind}{}",
                        burst
                            .slot
                            .map(|v| format!(" · slot {v}"))
                            .unwrap_or_default()
                    ),
                    fields: burst_fields,
                },
                now,
            );
            let slot_index = burst
                .slot
                .map(usize::from)
                .filter(|slot| *slot <= 2)
                .unwrap_or(0);
            if let Some(lc) = self.dmr_embedded_lc[slot_index].push(&burst) {
                let mut fields = BTreeMap::new();
                fields.insert("opcode".into(), format!("0x{:02X}", lc.opcode));
                fields.insert("featureId".into(), format!("0x{:02X}", lc.feature_id));
                fields.insert("target".into(), lc.target_id.to_string());
                fields.insert("source".into(), lc.source_id.to_string());
                fields.insert(
                    "serviceOptions".into(),
                    format!("0x{:02X}", lc.service_options),
                );
                fields.insert("fecCorrections".into(), lc.corrected_bits.to_string());
                fields.insert("raw".into(), lc.raw.clone());
                if let Some(alias) = &lc.talker_alias {
                    fields.insert("talkerAlias".into(), alias.clone());
                }
                if let (Some(latitude), Some(longitude)) = (lc.latitude, lc.longitude) {
                    fields.insert("latitude".into(), format!("{latitude:.5}"));
                    fields.insert("longitude".into(), format!("{longitude:.5}"));
                }
                if let Some(error) = lc.position_error_m {
                    fields.insert("positionErrorM".into(), error.to_string());
                }
                self.publish_decode(
                    DecodeEvent {
                        protocol: "DMR".into(),
                        kind: "embedded link control".into(),
                        summary: format!(
                            "{} · target {} · source {}",
                            lc.opcode_name, lc.target_id, lc.source_id
                        ),
                        valid: true,
                        fields,
                    },
                    now,
                );
            }
            if matches!(&burst.kind, BurstKind::Data { slot_type: Some(st), .. } if st.data_type == 3)
            {
                match decode_csbk(&burst) {
                    Ok(csbk) => {
                        let protocol = csbk.system.as_deref().unwrap_or("DMR");
                        self.last_dmr_match = Some((
                            source,
                            match protocol {
                                "DMR Tier III" => "Tier III control",
                                "Motorola Connect Plus" => "Connect Plus control",
                                "Motorola Capacity Plus" => "Capacity Plus control",
                                "Hytera XPT" => "XPT control",
                                _ => "trunk control",
                            },
                            Some(color_code),
                            burst.slot,
                            self.dmr_receiver.quality_db(),
                            now,
                        ));
                        let mut fields = BTreeMap::new();
                        fields.insert("opcode".into(), format!("0x{:02X}", csbk.opcode));
                        fields.insert("featureId".into(), format!("0x{:02X}", csbk.feature_id));
                        fields.insert("fecCorrections".into(), csbk.corrected_bits.to_string());
                        fields.insert("raw".into(), csbk.raw.clone());
                        if let Some(system) = &csbk.system {
                            fields.insert("system".into(), system.clone());
                        }
                        if let Some(call_type) = &csbk.call_type {
                            fields.insert("callType".into(), call_type.clone());
                        }
                        if let Some(v) = burst.slot {
                            fields.insert("burstSlot".into(), v.to_string());
                        }
                        fields.insert("colorCode".into(), color_code.to_string());
                        if let Some(v) = csbk.lcn {
                            fields.insert("lcn".into(), v.to_string());
                        }
                        if let Some(v) = csbk.timeslot {
                            fields.insert("grantedSlot".into(), v.to_string());
                        }
                        if let Some(v) = csbk.target_id {
                            fields.insert("target".into(), v.to_string());
                        }
                        if let Some(v) = csbk.source_id {
                            fields.insert("source".into(), v.to_string());
                        }
                        if let Some(v) = csbk.system_code {
                            fields.insert("systemCode".into(), format!("0x{v:04X}"));
                        }
                        if let Some(v) = csbk.announcement_type {
                            fields.insert("announcementType".into(), v.to_string());
                        }
                        if let Some(v) = csbk.rest_lsn {
                            fields.insert("restLsn".into(), v.to_string());
                        }
                        if !csbk.adjacent_sites.is_empty() {
                            fields.insert(
                                "adjacentSites".into(),
                                csbk.adjacent_sites
                                    .iter()
                                    .map(u8::to_string)
                                    .collect::<Vec<_>>()
                                    .join(", "),
                            );
                        }
                        fields.extend(csbk.vendor_fields.clone());
                        let mut parts = vec![csbk.opcode_name.clone()];
                        if let Some(lcn) = csbk.lcn {
                            parts.push(format!("LCN {lcn}/TS{}", csbk.timeslot.unwrap_or(1)));
                        }
                        if let Some(target) = csbk.target_id {
                            parts.push(format!("target {target}"));
                        }
                        if let Some(source) = csbk.source_id {
                            parts.push(format!("source {source}"));
                        }
                        if let Some(system) = csbk.system_code {
                            parts.push(format!("system 0x{system:04X}"));
                        }
                        self.publish_decode(
                            DecodeEvent {
                                protocol: protocol.into(),
                                kind: "CSBK".into(),
                                summary: parts.join(" · "),
                                valid: true,
                                fields,
                            },
                            now,
                        );
                    }
                    Err(error) => {
                        let (summary, fields) = match error {
                            CsbkError::Fec => (
                                "CSBK rejected: BPTC uncorrectable".to_string(),
                                BTreeMap::new(),
                            ),
                            CsbkError::Crc { raw } => {
                                let mut f = BTreeMap::new();
                                f.insert("raw".into(), raw);
                                ("CSBK rejected: CRC mismatch".to_string(), f)
                            }
                        };
                        self.publish_decode(
                            DecodeEvent {
                                protocol: "DMR".into(),
                                kind: "decode error".into(),
                                summary,
                                valid: false,
                                fields,
                            },
                            now,
                        );
                    }
                }
            } else if matches!(&burst.kind, BurstKind::Data { slot_type: Some(st), .. } if matches!(st.data_type, 0 | 1 | 2 | 6 | 7 | 8 | 10 | 11))
            {
                match decode_data_pdu(&burst) {
                    Ok(pdu) => {
                        // A header opens a message; the blocks that follow are
                        // what it actually said. Collect them before reporting,
                        // so the payload arrives with the envelope.
                        let slot = burst.slot.filter(|slot| matches!(slot, 1 | 2)).unwrap_or(0);
                        let assembler = &mut self.dmr_data[usize::from(slot)];
                        let assembled = match &pdu {
                            DataPdu::DataHeader(header) => {
                                assembler.begin(header.clone());
                                None
                            }
                            DataPdu::DataBlock(block) => assembler.push(block),
                            _ => None,
                        };
                        let (kind, summary, fields) = dmr_data_event(&pdu, &burst);
                        self.publish_decode(
                            DecodeEvent {
                                protocol: "DMR".into(),
                                kind,
                                summary,
                                valid: true,
                                fields,
                            },
                            now,
                        );
                        if let Some(message) = assembled {
                            let (summary, fields) = dmr_message_event(&message);
                            self.publish_decode(
                                DecodeEvent {
                                    protocol: "DMR".into(),
                                    kind: "data message".into(),
                                    summary,
                                    valid: message.crc_ok,
                                    fields,
                                },
                                now,
                            );
                        }
                    }
                    Err(DataError::Unsupported) => {}
                    Err(error) => {
                        let (summary, fields) = match error {
                            DataError::Fec => (
                                "data PDU rejected: BPTC uncorrectable".to_string(),
                                BTreeMap::new(),
                            ),
                            DataError::Checksum { raw } => {
                                let mut fields = BTreeMap::new();
                                fields.insert("raw".into(), raw);
                                ("data PDU rejected: checksum mismatch".to_string(), fields)
                            }
                            DataError::Unsupported => unreachable!(),
                        };
                        self.publish_decode(
                            DecodeEvent {
                                protocol: "DMR".into(),
                                kind: "decode error".into(),
                                summary,
                                valid: false,
                                fields,
                            },
                            now,
                        );
                    }
                }
            }
        }
        for frame in nxdn_frames {
            let control_messages = crate::nxdn::decode_control(&frame);
            let system = frame.system.label().to_string();
            self.last_nxdn_match = Some((
                frame.rate,
                system.clone(),
                frame.lich,
                frame.kind(),
                frame.correlation,
                now,
            ));
            let mut fields = BTreeMap::new();
            fields.insert("rate".into(), frame.rate.label().into());
            fields.insert("system".into(), system.clone());
            fields.insert("lich".into(), format!("0x{:02X}", frame.lich));
            fields.insert("rfChannel".into(), frame.rf_channel.to_string());
            fields.insert(
                "functionalChannel".into(),
                frame.functional_channel.to_string(),
            );
            fields.insert("option".into(), frame.option.to_string());
            fields.insert(
                "direction".into(),
                if frame.outbound {
                    "outbound"
                } else {
                    "inbound"
                }
                .into(),
            );
            fields.insert("voiceBlocks".into(), frame.voice_blocks.len().to_string());
            fields.insert("sync".into(), format!("{:.0}%", frame.correlation * 100.0));
            fields.insert("deviationHz".into(), format!("{:.0}", frame.deviation_hz));
            self.publish_decode(
                DecodeEvent {
                    protocol: if frame.system == crate::nxdn::System::TypeD {
                        "NXDN Type-D / IDAS".into()
                    } else if frame.is_control() {
                        "NXDN Type-C / Conventional".into()
                    } else {
                        frame.rate.label().into()
                    },
                    kind: frame.kind().to_ascii_lowercase(),
                    summary: format!(
                        "{} · {} · LICH 0x{:02X} · {}",
                        frame.rate.label(),
                        system,
                        frame.lich,
                        frame.kind()
                    ),
                    valid: true,
                    fields,
                },
                now,
            );
            for message in control_messages {
                let mut fields = BTreeMap::new();
                fields.insert("channel".into(), message.channel.clone());
                fields.insert(
                    "messageType".into(),
                    format!("0x{:02X}", message.message_type),
                );
                fields.insert("raw".into(), message.raw.clone());
                if let Some(value) = message.ran {
                    fields.insert("ran".into(), value.to_string());
                }
                if let Some(value) = message.source_id {
                    fields.insert("source".into(), value.to_string());
                }
                if let Some(value) = message.target_id {
                    fields.insert("target".into(), value.to_string());
                }
                if let Some(value) = message.group {
                    fields.insert("group".into(), value.to_string());
                }
                if let Some(value) = message.channel_number {
                    fields.insert("channelNumber".into(), value.to_string());
                }
                if let Some(value) = message.location_id {
                    fields.insert("locationId".into(), format!("0x{value:06X}"));
                }
                if let Some(value) = message.site_code {
                    fields.insert("site".into(), value.to_string());
                }
                if let Some(value) = message.cipher_type {
                    fields.insert("cipherType".into(), value.to_string());
                }
                if let Some(value) = message.key_id {
                    fields.insert("keyId".into(), value.to_string());
                }
                let mut summary = vec![message.message_name.clone()];
                if let Some(channel) = message.channel_number {
                    summary.push(format!("channel {channel}"));
                }
                if let Some(target) = message.target_id {
                    summary.push(format!("target {target}"));
                }
                if let Some(source) = message.source_id {
                    summary.push(format!("source {source}"));
                }
                self.publish_decode(
                    DecodeEvent {
                        protocol: if frame.system == crate::nxdn::System::TypeD {
                            "NXDN Type-D / IDAS".into()
                        } else {
                            "NXDN Type-C / Conventional".into()
                        },
                        kind: message.channel,
                        summary: summary.join(" · "),
                        valid: message.valid,
                        fields,
                    },
                    now,
                );
            }
        }
        for osw in smartnet_osws {
            self.last_smartnet_match = Some((
                osw.address,
                osw.opcode_name.clone(),
                osw.lcn,
                osw.corrected_bits,
                osw.sync_errors,
                now,
            ));
            let mut fields = osw.fields.clone();
            fields.insert("command".into(), format!("0x{:04X}", osw.command));
            fields.insert("fecCorrections".into(), osw.corrected_bits.to_string());
            fields.insert("syncErrors".into(), osw.sync_errors.to_string());
            fields.insert(
                "polarity".into(),
                if osw.inverted { "inverted" } else { "normal" }.into(),
            );
            fields.insert("raw".into(), osw.raw.clone());
            let mut summary = vec![osw.opcode_name.clone()];
            match osw.opcode {
                0x308 | 0x309 => summary.push(format!("talkgroup {}", osw.address)),
                0x30b => summary.push(format!("radio {}", osw.address)),
                0x31b => summary.push(format!("site {}", osw.address)),
                0x080 => summary.push(format!("system 0x{:04X}", osw.address)),
                _ => summary.push(format!("address {}", osw.address)),
            }
            if matches!(osw.opcode, 0x308 | 0x309 | 0x30b | 0x31b | 0x310) {
                summary.push(format!("LCN {}", osw.lcn));
            }
            self.publish_decode(
                DecodeEvent {
                    protocol: "Motorola SmartNet / SmartZone".into(),
                    kind: "OSW".into(),
                    summary: summary.join(" · "),
                    valid: true,
                    fields,
                },
                now,
            );
        }
        // A weak continuous P25 control channel can yield only occasional
        // BCH-valid frames. Once acquired, keep protocol ownership long
        // enough that embedded data cannot be reinterpreted as pager/DCS
        // traffic between those frames.
        let p25_locked = self.last_p25_match.is_some_and(|(_, _, _, _, at)| {
            now.duration_since(at) < std::time::Duration::from_secs(8)
        });
        if !p25_locked && let Some((baud, capcode)) = pocsag_hit {
            self.last_pocsag_match = Some((baud, capcode, now));
        }
        if !p25_locked {
            for message in std::mem::take(&mut self.pending_pocsag_messages) {
                let mut fields = BTreeMap::new();
                fields.insert("capcode".into(), message.capcode.to_string());
                fields.insert("function".into(), message.function.to_string());
                fields.insert("baud".into(), message.baud.to_string());
                fields.insert("partial".into(), message.partial.to_string());
                self.publish_decode(
                    DecodeEvent {
                        protocol: "POCSAG".into(),
                        kind: "page".into(),
                        valid: true,
                        summary: if message.text.is_empty() {
                            format!("capcode {} (tone only)", message.capcode)
                        } else {
                            format!("capcode {} · {}", message.capcode, message.text)
                        },
                        fields,
                    },
                    now,
                );
            }
        }
        if !p25_locked && let Some((baud, capcode)) = flex_hit {
            self.last_flex_match = Some((baud, capcode, now));
        }
        if !p25_locked {
            for message in std::mem::take(&mut self.pending_flex_messages) {
                let mut fields = BTreeMap::new();
                fields.insert("capcode".into(), message.capcode.to_string());
                fields.insert("cycle".into(), message.cycle.to_string());
                fields.insert("frame".into(), message.frame.to_string());
                fields.insert("baud".into(), message.baud.to_string());
                fields.insert("format".into(), format!("{:?}", message.format));
                self.publish_decode(
                    DecodeEvent {
                        protocol: "FLEX".into(),
                        kind: "page".into(),
                        valid: true,
                        summary: if message.text.is_empty() {
                            format!("capcode {}", message.capcode)
                        } else {
                            format!("capcode {} · {}", message.capcode, message.text)
                        },
                        fields,
                    },
                    now,
                );
            }
        }

        let analysis_due = self.samples_since_analysis >= (self.fs * 0.05) as usize;
        if active && analysis_due {
            self.samples_since_analysis = 0;
            // 7. Correlate APRS / AX.25 (Bell 202 AFSK 1200)
            if self.check_aprs(&self.analysis_buf) {
                self.last_aprs_match = Some(now);
            }

            // 8. Correlate NOAA SAME (520.83 baud AFSK)
            if self.check_same(&self.analysis_buf, tuned_freq_hz) {
                self.last_same_match = Some(now);
            }

            self.last_clocked = analyze_clocked_modulation(&self.analysis_buf, self.fs as f32);
        }

        if active {
            // 9. Detect CTCSS / DCS Subaudible Tones
            if !p25_locked && let Some(Some(det)) = self.ctcss.push(&self.demod.subaudible) {
                self.last_ctcss_match = Some((det.tone_hz, now));
            }
            if !p25_locked && let Some(Some(det)) = self.dcs.push(&self.demod.subaudible) {
                self.last_dcs_match = Some((det.code, now));
            }
            for word in self.ltr_decoder.process(&self.demod.subaudible) {
                // A nine-bit sync collision is possible in any busy digital
                // waveform. Preserve bad-CRC words only when the independent
                // clock/level detector also sees the 300-baud binary family.
                // CRC-valid Standard LTR words do not need this heuristic gate.
                if !matches!(self.last_clocked, Some(f) if f.baud == 300 && f.levels == 2) {
                    continue;
                }
                let mut fields = BTreeMap::new();
                fields.insert("area".into(), word.area.to_string());
                fields.insert("home".into(), word.home.to_string());
                fields.insert("group".into(), word.group.to_string());
                fields.insert("channel".into(), word.channel.to_string());
                fields.insert("free".into(), word.free.to_string());
                fields.insert(
                    "direction".into(),
                    if word.inbound { "inbound" } else { "outbound" }.into(),
                );
                fields.insert("raw".into(), word.raw.clone());
                let (protocol, kind, summary) = if word.crc_ok {
                    self.last_ltr_match = Some((
                        word.area,
                        word.home,
                        word.group,
                        word.channel,
                        word.free,
                        now,
                    ));
                    (
                        "LTR Standard",
                        "status word",
                        format!(
                            "{}-{:02}-{:03} · go-to {} · free {}",
                            word.area, word.home, word.group, word.channel, word.free
                        ),
                    )
                } else {
                    (
                        "LTR family",
                        "candidate",
                        format!("sync-shaped word {} · Standard LTR CRC failed", word.raw),
                    )
                };
                self.publish_decode(
                    DecodeEvent {
                        protocol: protocol.into(),
                        kind: kind.into(),
                        summary,
                        valid: word.crc_ok,
                        fields,
                    },
                    now,
                );
            }
            for word in self.passport_decoder.process(&self.demod.subaudible) {
                // Passport shares a short sync pattern with LTR Standard. Its
                // full checksum plus an independently measured 300-baud clock
                // are both required before publishing a protocol match.
                if !matches!(self.last_clocked, Some(f) if f.baud == 300 && f.levels == 2) {
                    continue;
                }
                self.last_passport_match = Some((
                    word.color_code,
                    word.channel,
                    word.site,
                    word.group,
                    word.message_name.clone(),
                    now,
                ));
                let mut fields = BTreeMap::new();
                fields.insert("digitalColorCode".into(), word.color_code.to_string());
                fields.insert("channel".into(), word.channel.to_string());
                fields.insert("site".into(), word.site.to_string());
                fields.insert("group".into(), word.group.to_string());
                fields.insert("radioId".into(), word.radio_id.to_string());
                fields.insert("messageType".into(), word.message_type.to_string());
                fields.insert("freeChannel".into(), word.free.to_string());
                fields.insert("raw".into(), word.raw.clone());
                self.publish_decode(
                    DecodeEvent {
                        protocol: "Passport".into(),
                        kind: word.message_name.clone(),
                        summary: format!(
                            "site {} · DCC {} · LCN {} · group {} · free {}",
                            word.site, word.color_code, word.channel, word.group, word.free
                        ),
                        valid: true,
                        fields,
                    },
                    now,
                );
            }
        }

        // 10. Prioritized Protocol Decision
        if let Some((nac, duid, correlation, corrected, t)) = self.last_p25_match
            && now.duration_since(t) < std::time::Duration::from_secs(8)
        {
            let duid_str = if corrected <= 3 {
                match duid {
                    0 => "Header (HDU)",
                    3 => "Terminator (TDU)",
                    5 => "Voice (LDU1)",
                    7 => "Control (TSDU)",
                    10 => "Voice (LDU2)",
                    12 => "Packet Data (PDU)",
                    15 => "Terminator (TDULC)",
                    _ => "P25 Frame",
                }
            } else {
                "Frame (DUID uncertain)"
            };
            return ClassificationResult {
                active: true,
                snr_db,
                rf_dbfs,
                peak_dev_hz,
                rms_dev_hz,
                center_offset_hz: mean_offset_hz,
                modulation: "4-FSK / C4FM @ 4800 Bd".into(),
                protocol: "P25 Phase 1".into(),
                details: Some(format!(
                    "NAC: ${nac:03X} · {duid_str} · sync {:.0}% · BCH +{corrected}",
                    correlation * 100.0
                )),
                confidence: (correlation - corrected as f32 * 0.025).clamp(0.55, 0.98),
            };
        }

        if let Some((correlation, hits, t)) = self.last_p25_candidate
            && hits >= 2
            && now.duration_since(t) < hold_duration
        {
            return ClassificationResult {
                active: true,
                snr_db,
                rf_dbfs,
                peak_dev_hz,
                rms_dev_hz,
                center_offset_hz: mean_offset_hz,
                modulation: "4-FSK / C4FM @ 4800 Bd".into(),
                protocol: "P25 Phase 1 (probable)".into(),
                details: Some(format!(
                    "{hits} repeated P25 frame syncs · NID FEC not yet valid · sync {:.0}%",
                    correlation * 100.0
                )),
                confidence: (0.62 + hits as f32 * 0.035 + (correlation - 0.72) * 0.4)
                    .clamp(0.68, 0.88),
            };
        }

        if let Some((source, kind, color_code, slot, quality_db, t)) = self.last_dmr_match
            && now.duration_since(t) < hold_duration
        {
            let mut metadata = vec![format!("{source} {kind} sync")];
            if let Some(slot) = slot {
                metadata.push(format!("Slot {slot}"));
            }
            if let Some(color_code) = color_code {
                metadata.push(format!("Color Code {color_code}"));
            }
            metadata.push(format!("quality {quality_db:.1} dB"));
            return ClassificationResult {
                active: true,
                snr_db,
                rf_dbfs,
                peak_dev_hz,
                rms_dev_hz,
                center_offset_hz: mean_offset_hz,
                modulation: "4-FSK @ 4800 Bd (TDMA)".into(),
                protocol: match kind {
                    "Tier III control" => "DMR Tier III",
                    "Connect Plus control" => "Motorola Connect Plus",
                    "Capacity Plus control" => "Motorola Capacity Plus",
                    "XPT control" => "Hytera XPT",
                    _ => "DMR / MotoTRBO",
                }
                .into(),
                details: Some(metadata.join(" · ")),
                confidence: (0.88 + quality_db.max(0.0).min(15.0) / 150.0).min(0.98),
            };
        }

        if let Some((rate, ref system, lich, kind, correlation, t)) = self.last_nxdn_match
            && now.duration_since(t) < hold_duration
        {
            return ClassificationResult {
                active: true,
                snr_db,
                rf_dbfs,
                peak_dev_hz,
                rms_dev_hz,
                center_offset_hz: mean_offset_hz,
                modulation: format!("4-FSK @ {} Bd", rate.symbol_rate() as u32),
                protocol: if system.contains("Type-D") {
                    "NXDN Type-D / IDAS".into()
                } else if kind == "Control" {
                    "NXDN Type-C / Conventional".into()
                } else {
                    rate.label().into()
                },
                details: Some(format!(
                    "{system} · {kind} · LICH 0x{lich:02X} · sync {:.0}%",
                    correlation * 100.0
                )),
                confidence: correlation.clamp(0.80, 0.98),
            };
        }

        if let Some((ref frame, t)) = self.last_legacy_match
            && now.duration_since(t) < hold_duration
        {
            return ClassificationResult {
                active: true,
                snr_db,
                rf_dbfs,
                peak_dev_hz,
                rms_dev_hz,
                center_offset_hz: mean_offset_hz,
                modulation: format!("{} @ {} Bd", frame.modulation, frame.baud),
                protocol: frame.protocol.into(),
                details: Some(format!(
                    "{} · {} polarity · sync errors {}{}",
                    frame.kind,
                    if frame.inverted { "inverted" } else { "normal" },
                    frame.sync_errors,
                    if frame.cadence_hits > 1 {
                        format!(" · {} frames at expected cadence", frame.cadence_hits)
                    } else {
                        String::new()
                    }
                )),
                confidence: if frame.cadence_hits >= 2 { 0.98 } else { 0.94 },
            };
        }

        if let Some((address, ref opcode, lcn, corrected, sync_errors, t)) =
            self.last_smartnet_match
            && now.duration_since(t) < hold_duration
        {
            return ClassificationResult {
                active: true,
                snr_db,
                rf_dbfs,
                peak_dev_hz,
                rms_dev_hz,
                center_offset_hz: mean_offset_hz,
                modulation: "MSK / binary FSK @ 3600 Bd".into(),
                protocol: "Motorola SmartNet / SmartZone".into(),
                details: Some(format!(
                    "{opcode} · address {address} · LCN {lcn} · sync errors {sync_errors} · BCH +{corrected}"
                )),
                confidence: (0.98 - sync_errors as f32 * 0.04 - corrected as f32 * 0.015)
                    .clamp(0.72, 0.98),
            };
        }

        if let Some((baud, capcode, t)) = self.last_pocsag_match
            && now.duration_since(t) < hold_duration
        {
            let cap_str = if capcode > 0 {
                format!(" · Cap: {capcode:07}")
            } else {
                String::new()
            };
            return ClassificationResult {
                active: true,
                snr_db,
                rf_dbfs,
                peak_dev_hz,
                rms_dev_hz,
                center_offset_hz: mean_offset_hz,
                modulation: format!("2-FSK @ {baud} Bd (±4.5 kHz)"),
                protocol: "POCSAG Pager".into(),
                details: Some(format!("{baud} Baud{cap_str}")),
                confidence: 0.98,
            };
        }

        if let Some((color_code, channel, site, group, ref message_name, t)) =
            self.last_passport_match
            && now.duration_since(t) < hold_duration
        {
            return ClassificationResult {
                active: true,
                snr_db,
                rf_dbfs,
                peak_dev_hz,
                rms_dev_hz,
                center_offset_hz: mean_offset_hz,
                modulation: "Sub-audible binary FSK @ 300 Bd".into(),
                protocol: "Passport".into(),
                details: Some(format!(
                    "site {site} · DCC {color_code} · LCN {channel} · group {group} · {message_name} · checksum valid"
                )),
                confidence: 0.98,
            };
        }

        if let Some((area, home, group, channel, free, t)) = self.last_ltr_match
            && now.duration_since(t) < hold_duration
        {
            return ClassificationResult {
                active: true,
                snr_db,
                rf_dbfs,
                peak_dev_hz,
                rms_dev_hz,
                center_offset_hz: mean_offset_hz,
                modulation: "Sub-audible binary FSK @ 300 Bd".into(),
                protocol: "LTR Standard".into(),
                details: Some(format!(
                    "{area}-{home:02}-{group:03} · go-to {channel} · free {free} · CRC valid"
                )),
                confidence: 0.98,
            };
        }

        if let Some((baud, capcode, t)) = self.last_flex_match
            && now.duration_since(t) < hold_duration
        {
            let cap = if capcode > 0 {
                format!(" · Cap: {capcode}")
            } else {
                String::new()
            };
            return ClassificationResult {
                active: true,
                snr_db,
                rf_dbfs,
                peak_dev_hz,
                rms_dev_hz,
                center_offset_hz: mean_offset_hz,
                modulation: format!("2/4-FSK @ {baud} bps"),
                protocol: "FLEX Pager".into(),
                details: Some(format!("Motorola FLEX frame sync{cap}")),
                confidence: 0.95,
            };
        }

        if let Some(t) = self.last_aprs_match
            && now.duration_since(t) < hold_duration
        {
            let aprs_channel = (tuned_freq_hz - 144_390_000.0).abs() < 8_000.0;
            return ClassificationResult {
                active: true,
                snr_db,
                rf_dbfs,
                peak_dev_hz,
                rms_dev_hz,
                center_offset_hz: mean_offset_hz,
                modulation: "AFSK @ 1200 Bd (Bell 202)".into(),
                protocol: if aprs_channel {
                    "APRS / AX.25 (probable)".into()
                } else {
                    "Bell 202 / AX.25 (probable)".into()
                },
                details: Some("Alternating 1200/2200 Hz Bell 202 symbols".into()),
                confidence: if aprs_channel { 0.91 } else { 0.84 },
            };
        }

        if let Some(t) = self.last_same_match
            && now.duration_since(t) < hold_duration
        {
            return ClassificationResult {
                active: true,
                snr_db,
                rf_dbfs,
                peak_dev_hz,
                rms_dev_hz,
                center_offset_hz: mean_offset_hz,
                modulation: "AFSK @ 520.83 Bd".into(),
                protocol: "NOAA SAME Weather".into(),
                details: Some("Emergency Alert Header / 1050 Hz Tone".into()),
                confidence: 0.95,
            };
        }

        // Check CTCSS / DCS Analog FM
        if let Some((tone_hz, t)) = self.last_ctcss_match
            && now.duration_since(t) < hold_duration
        {
            return ClassificationResult {
                active: true,
                snr_db,
                rf_dbfs,
                peak_dev_hz,
                rms_dev_hz,
                center_offset_hz: mean_offset_hz,
                modulation: "Analog NBFM (FM Voice)".into(),
                protocol: "Analog FM (CTCSS)".into(),
                details: Some(format!("PL Tone: {tone_hz:.1} Hz")),
                confidence: 0.92,
            };
        }

        if let Some((code, t)) = self.last_dcs_match
            && now.duration_since(t) < hold_duration
        {
            return ClassificationResult {
                active: true,
                snr_db,
                rf_dbfs,
                peak_dev_hz,
                rms_dev_hz,
                center_offset_hz: mean_offset_hz,
                modulation: "Analog NBFM (FM Voice)".into(),
                protocol: "Analog FM (DCS)".into(),
                details: Some(format!("DCS Code: D{code:03o}")),
                confidence: 0.92,
            };
        }

        // If no prior protocol match is within hold duration, check if carrier is active
        if !active {
            return ClassificationResult {
                active: false,
                snr_db,
                rf_dbfs,
                peak_dev_hz,
                rms_dev_hz,
                center_offset_hz: mean_offset_hz,
                modulation: "Noise".into(),
                protocol: "Idle / Noise".into(),
                details: None,
                confidence: 0.0,
            };
        }

        // Unmodulated Carrier / CW check
        if rms_dev_hz < 200.0 && peak_dev_hz < 450.0 {
            return ClassificationResult {
                active: true,
                snr_db,
                rf_dbfs,
                peak_dev_hz,
                rms_dev_hz,
                center_offset_hz: mean_offset_hz,
                modulation: "CW / Unmodulated".into(),
                protocol: "Carrier / Beacon".into(),
                details: Some("Constant unmodulated carrier".into()),
                confidence: 0.88,
            };
        }

        // Clocked-modulation analysis is deliberately below exact protocol
        // matchers. It expands coverage without turning every 4-FSK carrier into
        // a confidently-but-incorrectly named protocol.
        if let Some(features) = self.last_clocked {
            return classify_clocked_signal(
                features,
                tuned_freq_hz,
                snr_db,
                rf_dbfs,
                peak_dev_hz,
                rms_dev_hz,
                mean_offset_hz,
            );
        }

        // A high-deviation signal with no recoverable symbol clock is still
        // useful information, but it is not sufficient evidence for paging.
        if peak_dev_hz >= 3000.0 && rms_dev_hz > 1800.0 {
            ClassificationResult {
                active: true,
                snr_db,
                rf_dbfs,
                peak_dev_hz,
                rms_dev_hz,
                center_offset_hz: mean_offset_hz,
                modulation: "Unknown narrowband modulation".into(),
                protocol: "Unclassified Digital / Data".into(),
                details: Some(format!(
                    "Dev: Peak {:.1} kHz · RMS {:.1} kHz",
                    peak_dev_hz / 1e3,
                    rms_dev_hz / 1e3
                )),
                confidence: 0.42,
            }
        } else if peak_dev_hz >= 1000.0 && rms_dev_hz > 650.0 {
            ClassificationResult {
                active: true,
                snr_db,
                rf_dbfs,
                peak_dev_hz,
                rms_dev_hz,
                center_offset_hz: mean_offset_hz,
                modulation: "Unknown narrowband modulation".into(),
                protocol: "Unclassified Digital / Voice".into(),
                details: Some(format!(
                    "Dev: Peak {:.1} kHz · RMS {:.1} kHz",
                    peak_dev_hz / 1e3,
                    rms_dev_hz / 1e3
                )),
                confidence: 0.38,
            }
        } else {
            ClassificationResult {
                active: true,
                snr_db,
                rf_dbfs,
                peak_dev_hz,
                rms_dev_hz,
                center_offset_hz: mean_offset_hz,
                modulation: "Analog NBFM".into(),
                protocol: "Analog Voice FM".into(),
                details: Some(format!(
                    "Dev: Peak {:.1} kHz · RMS {:.1} kHz",
                    peak_dev_hz / 1e3,
                    rms_dev_hz / 1e3
                )),
                confidence: 0.65,
            }
        }
    }

    // --- APRS / AX.25 Matcher ---
    fn check_aprs(&self, disc: &[f32]) -> bool {
        // Bell 202: 1200 Hz mark, 2200 Hz space, 1200 baud
        if disc.len() < 256 {
            return false;
        }
        let window = recent_window(disc, (self.fs * 0.25) as usize);
        two_tone_fsk(window, 1200.0, 2200.0, 1200.0, self.fs as f32)
    }

    // --- NOAA SAME Matcher ---
    fn check_same(&self, disc: &[f32], freq_hz: f64) -> bool {
        // SAME tones: 2083.3 Hz and 1562.5 Hz
        if disc.len() < 256 {
            return false;
        }
        let window = recent_window(disc, (self.fs * 0.25) as usize);
        let weather_channel = (162_400_000.0..=162_550_000.0).contains(&freq_hz);
        two_tone_fsk(window, 1562.5, 2083.3, 520.83, self.fs as f32)
            && (weather_channel || signal_rms(window) > 500.0)
    }
}

#[derive(Clone, Copy, Debug)]
struct ClockedFeatures {
    baud: u32,
    clock_score: f32,
    levels: u8,
    level_quality: f32,
}

/// Derive an offset-independent FSK center and inner/outer slicing boundary.
fn fsk_slicer(samples: &[f32], fraction: f32) -> (f32, f32) {
    if samples.is_empty() {
        return (0.0, 900.0);
    }
    let mut sorted: Vec<f32> = samples.iter().copied().filter(|x| x.is_finite()).collect();
    if sorted.is_empty() {
        return (0.0, 900.0);
    }
    // Two order statistics are read — the median and the 90th percentile of
    // |x − median| — so partitions replace both sorts.
    let mid = sorted.len() / 2;
    sorted.select_nth_unstable_by(mid, f32::total_cmp);
    let center = sorted[mid];
    let mut deviations: Vec<f32> = sorted.iter().map(|x| (x - center).abs()).collect();
    let outer_idx = ((deviations.len() as f32 * 0.90) as usize).min(deviations.len() - 1);
    deviations.select_nth_unstable_by(outer_idx, f32::total_cmp);
    let outer = deviations[outer_idx];
    (center, (outer * fraction).clamp(250.0, 2600.0))
}

fn recent_window(samples: &[f32], max_len: usize) -> &[f32] {
    &samples[samples.len().saturating_sub(max_len)..]
}

fn signal_rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|x| x * x).sum::<f32>() / samples.len().max(1) as f32).sqrt()
}

/// Detect alternating FSK tones in symbol-sized windows. A single Goertzel over
/// an entire packet cancels as symbol phases and tones change, which was the
/// reason short APRS/SAME bursts were frequently missed.
fn two_tone_fsk(samples: &[f32], f1: f32, f2: f32, baud: f32, fs: f32) -> bool {
    let symbol_len = (fs / baud).round().max(16.0) as usize;
    // 300 Hz used to be the sole weak-signal gate; with the tonal-dominance
    // test below already requiring sustained mark/space separation, a lower
    // absolute floor lets marginal-but-real packets through while noise
    // still fails the consistency check.
    if samples.len() < symbol_len * 8 || signal_rms(samples) < 200.0 {
        return false;
    }
    for offset in [0, symbol_len / 2] {
        let mut first = 0usize;
        let mut second = 0usize;
        let mut tonal = 0usize;
        let mut total = 0usize;
        for chunk in samples[offset..].chunks_exact(symbol_len) {
            let rms = signal_rms(chunk).max(1.0);
            let a = goertzel_mag_centered(chunk, f1, fs);
            let b = goertzel_mag_centered(chunk, f2, fs);
            if a.max(b) > rms * 0.48 {
                tonal += 1;
                if a > b * 1.20 {
                    first += 1;
                } else if b > a * 1.20 {
                    second += 1;
                }
            }
            total += 1;
        }
        if first >= 3 && second >= 3 && tonal * 5 >= total * 2 {
            return true;
        }
    }
    false
}

/// Look for transition energy aligned to a symbol clock, then measure whether
/// samples taken at symbol centers form two or four stable deviation levels.
fn analyze_clocked_modulation(samples: &[f32], fs: f32) -> Option<ClockedFeatures> {
    const CANDIDATES: [u32; 8] = [300, 512, 600, 1200, 1600, 2400, 4800, 9600];
    const MAX_PHASE_BINS: usize = 20;

    let samples = recent_window(samples, (fs * 0.35) as usize);
    if samples.len() < (fs * 0.08) as usize {
        return None;
    }
    let (center, _) = fsk_slicer(samples, 0.5);
    let mut best = (0u32, 0.0f32, 0usize);

    for baud in CANDIDATES {
        // Do not create more phase bins than samples/symbol. Empty bins would
        // make ordinary sampled noise look periodic at high candidate rates.
        let phase_bins = MAX_PHASE_BINS.min((fs / baud as f32).floor().max(3.0) as usize);
        let mut bins = [0.0f32; MAX_PHASE_BINS];
        let mut counts = [0u32; MAX_PHASE_BINS];
        let rate = baud as f32 / fs;
        for i in 2..samples.len() {
            let a = (samples[i] - center).clamp(-8000.0, 8000.0);
            let b = (samples[i - 2] - center).clamp(-8000.0, 8000.0);
            let edge = (a - b).abs().min(6000.0);
            let phase = ((i as f32 * rate).fract() * phase_bins as f32) as usize;
            bins[phase.min(phase_bins - 1)] += edge;
            counts[phase.min(phase_bins - 1)] += 1;
        }
        for phase in 0..phase_bins {
            bins[phase] /= counts[phase].max(1) as f32;
        }
        let mean = bins[..phase_bins].iter().sum::<f32>() / phase_bins as f32;
        if mean <= 1.0 {
            continue;
        }

        // Locate the symbol-period "knee": differences should grow from half a
        // symbol to one symbol, then largely saturate by two symbols. This
        // rejects both subharmonics and the tempting 2x clock harmonic.
        let half_lag = (fs / (baud as f32 * 2.0)).round().max(1.0) as usize;
        let one_lag = (fs / baud as f32).round().max(1.0) as usize;
        let two_lag = one_lag * 2;
        let d_half = mean_lag_difference(samples, half_lag);
        let d_one = mean_lag_difference(samples, one_lag);
        let d_two = mean_lag_difference(samples, two_lag);
        let rise = d_one / d_half.max(1.0);
        let saturation = d_two / d_one.max(1.0);
        if rise < 1.12 || saturation > 1.48 {
            continue;
        }
        let mut peak = 0.0f32;
        let mut peak_phase = 0usize;
        for phase in 0..phase_bins {
            let local = (bins[(phase + phase_bins - 1) % phase_bins]
                + bins[phase]
                + bins[(phase + 1) % phase_bins])
                / 3.0;
            if local > peak {
                peak = local;
                peak_phase = phase;
            }
        }
        let score = peak / mean;
        if score > best.1 {
            best = (baud, score, peak_phase);
        }
    }

    // A uniform derivative distribution is analog/noise. The threshold is low
    // enough for Gaussian-filtered MSK, whose transitions are intentionally soft.
    if best.1 < 1.28 {
        return None;
    }

    let baud = best.0;
    let sps = fs / baud as f32;
    let phase_bins = MAX_PHASE_BINS.min((fs / baud as f32).floor().max(3.0) as usize);
    let boundary = best.2 as f32 / phase_bins as f32;
    let mut symbols = Vec::with_capacity((samples.len() as f32 / sps) as usize);
    let first_center = ((boundary + 0.5) * sps).rem_euclid(sps);
    let mut pos = first_center;
    while pos < samples.len() as f32 {
        let idx = pos.round() as usize;
        if idx < samples.len() {
            symbols.push(samples[idx] - center);
        }
        pos += sps;
    }
    if symbols.len() < 32 {
        return None;
    }

    // Trim impulsive discriminator spikes before level clustering.
    symbols.sort_by(f32::total_cmp);
    let trim = symbols.len() / 50;
    let symbols = &symbols[trim..symbols.len().saturating_sub(trim).max(trim + 1)];
    let variance = signal_rms(symbols).powi(2).max(1.0);
    let q2 = (1.0 - kmeans_sse(symbols, 2) / (variance * symbols.len() as f32)).clamp(0.0, 1.0);
    let q4 = (1.0 - kmeans_sse(symbols, 4) / (variance * symbols.len() as f32)).clamp(0.0, 1.0);

    let (levels, quality) = if q2 >= 0.88 {
        (2, q2)
    } else if q4 >= 0.82 && q4 - q2 >= 0.08 {
        (4, q4)
    } else if q2 >= 0.68 {
        // Soft Gaussian shaping can smear binary symbols between the two rails.
        (2, q2)
    } else {
        return None;
    };

    Some(ClockedFeatures {
        baud,
        clock_score: best.1,
        levels,
        level_quality: quality,
    })
}

fn mean_lag_difference(samples: &[f32], lag: usize) -> f32 {
    if lag == 0 || samples.len() <= lag {
        return 0.0;
    }
    samples[lag..]
        .iter()
        .zip(&samples[..samples.len() - lag])
        .map(|(a, b)| (a - b).abs().min(12_000.0))
        .sum::<f32>()
        / (samples.len() - lag) as f32
}

fn kmeans_sse(samples: &[f32], k: usize) -> f32 {
    if samples.is_empty() {
        return f32::INFINITY;
    }
    let mut centers: Vec<f32> = (0..k)
        .map(|i| samples[((2 * i + 1) * samples.len() / (2 * k)).min(samples.len() - 1)])
        .collect();
    for _ in 0..8 {
        let mut sums = vec![0.0f32; k];
        let mut counts = vec![0u32; k];
        for &x in samples {
            let (idx, _) = centers
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| (x - **a).abs().total_cmp(&(x - **b).abs()))
                .unwrap();
            sums[idx] += x;
            counts[idx] += 1;
        }
        for i in 0..k {
            if counts[i] > 0 {
                centers[i] = sums[i] / counts[i] as f32;
            }
        }
    }
    samples
        .iter()
        .map(|&x| {
            centers
                .iter()
                .map(|&c| (x - c) * (x - c))
                .min_by(f32::total_cmp)
                .unwrap_or(0.0)
        })
        .sum()
}

/// A reassembled data message, with its payload front and centre.
fn dmr_message_event(m: &DataMessage) -> (String, BTreeMap<String, String>) {
    let mut fields = BTreeMap::new();
    fields.insert("service".into(), m.header.service_name.clone());
    fields.insert("format".into(), m.header.format_name.clone());
    fields.insert("target".into(), m.header.target_id.to_string());
    fields.insert("source".into(), m.header.source_id.to_string());
    fields.insert("blocks".into(), m.blocks.to_string());
    fields.insert("confirmed".into(), m.header.confirmed.to_string());
    fields.insert(
        if m.header.udt.is_some() {
            "crc16"
        } else {
            "crc32"
        }
        .into(),
        if m.crc_ok { "ok" } else { "bad" }.into(),
    );
    fields.insert("payloadOctets".into(), m.payload.len().to_string());
    fields.insert("payloadHex".into(), m.payload_hex.clone());
    if let Some(encoding) = &m.encoding {
        fields.insert("encoding".into(), encoding.clone());
    }
    if let Some(text) = &m.text {
        fields.insert("text".into(), text.clone());
    }
    if let Some(application) = &m.application {
        fields.insert("application".into(), application.kind.clone());
        fields.extend(application.fields.clone());
    }
    let body = match (&m.application, &m.text) {
        (Some(application), _) => application.summary.clone(),
        (_, Some(text)) => format!("\u{201c}{text}\u{201d}"),
        _ => format!("{} octets, no readable text", m.payload.len()),
    };
    (
        format!(
            "{} \u{b7} {} \u{2192} {} \u{b7} {body}{}",
            m.header.service_name,
            m.header.source_id,
            m.header.target_id,
            if m.crc_ok { "" } else { " (CRC bad)" }
        ),
        fields,
    )
}

fn dmr_data_event(
    pdu: &DataPdu,
    burst: &crate::dmr::Burst,
) -> (String, String, BTreeMap<String, String>) {
    let mut fields = BTreeMap::new();
    if let Some(slot) = burst.slot {
        fields.insert("slot".into(), slot.to_string());
    }
    if let Some(color_code) = burst.color_code() {
        fields.insert("colorCode".into(), color_code.to_string());
    }
    match pdu {
        DataPdu::LinkControl(lc) => {
            fields.insert("opcode".into(), format!("0x{:02X}", lc.opcode));
            fields.insert("manufacturer".into(), lc.manufacturer.clone());
            fields.insert("featureId".into(), format!("0x{:02X}", lc.feature_id));
            fields.insert("target".into(), lc.target_id.to_string());
            fields.insert("source".into(), lc.source_id.to_string());
            fields.insert(
                "serviceOptions".into(),
                format!("0x{:02X}", lc.service_options),
            );
            fields.insert("emergency".into(), lc.emergency.to_string());
            fields.insert("encrypted".into(), lc.encrypted.to_string());
            fields.insert("priority".into(), lc.priority.to_string());
            fields.insert("fecCorrections".into(), lc.corrected_bits.to_string());
            fields.insert("raw".into(), lc.raw.clone());
            if let Some(rest) = lc.capacity_plus_rest_lsn {
                fields.insert("restLsn".into(), rest.to_string());
            }
            (
                "full link control".into(),
                format!(
                    "{} · target {} · source {}{}",
                    lc.opcode_name,
                    lc.target_id,
                    lc.source_id,
                    if lc.encrypted { " · encrypted" } else { "" }
                ),
                fields,
            )
        }
        DataPdu::PrivacyHeader(pi) => {
            fields.insert("algorithmId".into(), format!("0x{:02X}", pi.algorithm_id));
            fields.insert("algorithm".into(), pi.algorithm_name.clone());
            fields.insert("keyId".into(), format!("0x{:02X}", pi.key_id));
            fields.insert("manufacturer".into(), pi.manufacturer.clone());
            fields.insert("messageIndicator".into(), pi.message_indicator.clone());
            fields.insert("target".into(), pi.target_id.to_string());
            fields.insert("fecCorrections".into(), pi.corrected_bits.to_string());
            fields.insert("raw".into(), pi.raw.clone());
            (
                "privacy header".into(),
                format!(
                    "{} · ALG 0x{:02X} · KID 0x{:02X} · target {}",
                    pi.algorithm_name, pi.algorithm_id, pi.key_id, pi.target_id
                ),
                fields,
            )
        }
        DataPdu::DataBlock(block) => {
            fields.insert("rate".into(), block.rate.clone());
            if let Some(serial) = block.serial {
                fields.insert("blockSerial".into(), serial.to_string());
            }
            if let Some(ok) = block.block_crc_ok {
                fields.insert("blockCrc".into(), if ok { "ok" } else { "bad" }.into());
            }
            fields.insert("fecCorrections".into(), block.corrected_bits.to_string());
            fields.insert("raw".into(), block.raw.clone());
            (
                "data block".into(),
                format!(
                    "rate {} · {} octets{}",
                    block.rate,
                    block.payload.len(),
                    match block.serial {
                        Some(n) => format!(" · block {n}"),
                        None => String::new(),
                    }
                ),
                fields,
            )
        }
        DataPdu::DataHeader(header) => {
            fields.insert("format".into(), header.format.to_string());
            fields.insert("service".into(), header.service_name.clone());
            fields.insert("sap".into(), header.service_access_point.to_string());
            fields.insert("target".into(), header.target_id.to_string());
            fields.insert("source".into(), header.source_id.to_string());
            fields.insert("confirmed".into(), header.confirmed.to_string());
            fields.insert("fecCorrections".into(), header.corrected_bits.to_string());
            fields.insert("raw".into(), header.raw.clone());
            if let Some(blocks) = header.blocks_to_follow {
                fields.insert("blocksToFollow".into(), blocks.to_string());
            }
            if let Some(udt) = &header.udt {
                fields.insert("udtFormat".into(), udt.format_name.clone());
                fields.insert("udtOpcode".into(), udt.opcode.to_string());
                fields.insert("padNibbles".into(), udt.pad_nibbles.to_string());
                fields.insert("protected".into(), udt.protected.to_string());
            }
            (
                "data header".into(),
                format!(
                    "{} / {} · target {} · source {}",
                    header.format_name, header.service_name, header.target_id, header.source_id
                ),
                fields,
            )
        }
        DataPdu::UnifiedSingleBlockData(usbd) => {
            fields.insert("serviceType".into(), usbd.service_type.to_string());
            fields.insert("service".into(), usbd.service_name.clone());
            fields.insert("fecCorrections".into(), usbd.corrected_bits.to_string());
            fields.insert("raw".into(), usbd.raw.clone());
            if let Some(latitude) = usbd.latitude {
                fields.insert("latitude".into(), format!("{latitude:.6}"));
            }
            if let Some(longitude) = usbd.longitude {
                fields.insert("longitude".into(), format!("{longitude:.6}"));
            }
            if let Some(error) = usbd.position_error_m {
                fields.insert("positionErrorM".into(), error.to_string());
            }
            if let Some(speed) = usbd.speed_kph {
                fields.insert("speedKph".into(), format!("{speed:.1}"));
            }
            if let Some(direction) = usbd.direction_degrees {
                fields.insert("directionDegrees".into(), format!("{direction:.1}"));
            }
            if let Some(elapsed) = &usbd.time_elapsed {
                fields.insert("timeElapsed".into(), elapsed.clone());
            }
            if let Some(reason) = usbd.reason {
                fields.insert("reason".into(), reason.to_string());
            }
            if let Some(source_hash) = usbd.source_hash {
                fields.insert("sourceHash".into(), source_hash.to_string());
            }
            let location = match (usbd.latitude, usbd.longitude) {
                (Some(lat), Some(lon)) => format!(" · {lat:.5}, {lon:.5}"),
                _ => String::new(),
            };
            (
                "unified single block data".into(),
                format!("{}{}", usbd.service_name, location),
                fields,
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn classify_clocked_signal(
    f: ClockedFeatures,
    tuned_freq_hz: f64,
    snr_db: f32,
    rf_dbfs: f32,
    peak_dev_hz: f32,
    rms_dev_hz: f32,
    center_offset_hz: f32,
) -> ClassificationResult {
    let modulation = if f.levels == 4 {
        format!("4-FSK @ {} Bd", f.baud)
    } else {
        format!("2-FSK / GMSK @ {} Bd", f.baud)
    };
    let marine_ais = (tuned_freq_hz - 161_975_000.0).abs() < 8_000.0
        || (tuned_freq_hz - 162_025_000.0).abs() < 8_000.0;
    let (protocol, details) = match (f.baud, f.levels) {
        (4800, 4) => (
            "4-FSK Digital Voice / Data",
            "Candidates: P25, DMR, NXDN96, YSF, M17; awaiting frame sync".to_string(),
        ),
        (4800, 2) => (
            "GMSK Digital Voice / Data",
            "Candidates: D-STAR or 4800-baud packet; awaiting frame sync".to_string(),
        ),
        (2400, 4) => (
            "NXDN48 / 4-FSK (probable)",
            "2400-baud 4-level clock; protocol sync not yet confirmed".to_string(),
        ),
        (9600, 2) if marine_ais => (
            "AIS Marine Data (probable)",
            "9600-baud GMSK on an AIS channel; packet sync not yet confirmed".to_string(),
        ),
        (9600, _) => (
            "9600-baud Packet / Telemetry",
            "Candidates: packet radio, telemetry, or control data".to_string(),
        ),
        (512, 2) => (
            "POCSAG 512 (probable)",
            "512-baud binary FSK; awaiting pager sync".to_string(),
        ),
        (1200, 2) => (
            "1200-baud FSK / Packet",
            "Candidates: POCSAG, packet radio, MDC/data burst, telemetry".to_string(),
        ),
        (1600, _) => (
            "FLEX / Paging (probable)",
            "1600-baud clock detected; awaiting FLEX frame sync".to_string(),
        ),
        (2400, 2) => (
            "2400-baud FSK / Paging",
            "Candidates: POCSAG, telemetry, or narrowband data".to_string(),
        ),
        (300, 2) => (
            "LTR / Passport-family signalling (probable)",
            "300-baud sub-audible binary data; awaiting a CRC-valid LTR word or proprietary Passport framing".to_string(),
        ),
        (300 | 600, _) => (
            "Low-rate FSK / Telemetry",
            "Candidates: alerting, telemetry, selective calling, or legacy data".to_string(),
        ),
        _ => (
            "Clocked Digital Signal",
            "Symbol clock detected; no protocol frame sync yet".to_string(),
        ),
    };
    let confidence =
        (0.48 + (f.clock_score - 1.28).min(0.8) * 0.22 + f.level_quality * 0.22).clamp(0.50, 0.86);
    ClassificationResult {
        active: true,
        snr_db,
        rf_dbfs,
        peak_dev_hz,
        rms_dev_hz,
        center_offset_hz,
        modulation,
        protocol: protocol.into(),
        details: Some(format!(
            "{details} · clock {:.2} · levels {:.0}%",
            f.clock_score,
            f.level_quality * 100.0
        )),
        confidence,
    }
}

fn goertzel_mag_centered(samples: &[f32], target_freq: f32, fs: f32) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let mean = samples.iter().sum::<f32>() / samples.len() as f32;
    let k = (0.5 + (samples.len() as f32 * target_freq / fs)).floor();
    let w = (2.0 * PI / samples.len() as f32) * k;
    let coeff = 2.0 * w.cos();
    let mut q1 = 0.0f32;
    let mut q2 = 0.0f32;
    for &x in samples {
        let q0 = coeff * q1 - q2 + (x - mean);
        q2 = q1;
        q1 = q0;
    }
    (q1 * q1 + q2 * q2 - q1 * q2 * coeff).sqrt() / samples.len() as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    const FS: f64 = 48_000.0;

    fn fsk_iq(baud: u32, levels: &[f32], symbols: usize, offset_hz: f32) -> Vec<Complex32> {
        let sps = FS as usize / baud as usize;
        assert!(sps > 0);
        let mut phase = 0.0f32;
        let mut state = 0x5A17u32;
        let mut out = Vec::with_capacity(symbols * sps);
        for _ in 0..symbols {
            // Deterministic maximal-ish sequence avoids a test that depends on RNG.
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let freq = offset_hz + levels[(state as usize >> 16) % levels.len()];
            for _ in 0..sps {
                phase += TAU * freq / FS as f32;
                out.push(Complex32::new(phase.cos() * 0.5, phase.sin() * 0.5));
            }
        }
        out
    }

    fn afsk_iq(symbols: usize) -> Vec<Complex32> {
        let sps = FS as usize / 1200;
        let mut rf_phase = 0.0f32;
        let mut tone_phase = 0.0f32;
        let mut state = 0x91C3u32;
        let mut out = Vec::with_capacity(symbols * sps);
        for _ in 0..symbols {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let tone = if state & 0x8000 != 0 { 1200.0 } else { 2200.0 };
            for _ in 0..sps {
                tone_phase += TAU * tone / FS as f32;
                let instantaneous_hz = 2200.0 * tone_phase.sin();
                rf_phase += TAU * instantaneous_hz / FS as f32;
                out.push(Complex32::new(rf_phase.cos() * 0.5, rf_phase.sin() * 0.5));
            }
        }
        out
    }

    fn p25_frame_iq(nac: u16, duid: u8, offset_hz: f32) -> Vec<Complex32> {
        use crate::p25::{FRAME_SYNC, NID_SYMBOLS, SPS, bch_encode, dibit_level};

        let mut data: Vec<u8> = (0..24)
            .map(|i| ((FRAME_SYNC >> (46 - 2 * i)) & 0b11) as u8)
            .collect();
        let nid = bch_encode((nac << 4) | u16::from(duid)) << 1;
        for k in 0..NID_SYMBOLS {
            data.push(((nid >> (62 - 2 * k)) & 0b11) as u8);
        }
        let mut frame = Vec::new();
        let mut taken = 0usize;
        let mut idx = 0usize;
        while taken < data.len() {
            if (idx + 1) % 36 == 0 {
                frame.push(0b01);
            } else {
                frame.push(data[taken]);
                taken += 1;
            }
            idx += 1;
        }
        let mut dibits = Vec::new();
        for i in 0..160 {
            dibits.push([0b01, 0b11, 0b00, 0b10][i % 4]);
        }
        dibits.extend(frame);
        for i in 0..80 {
            dibits.push([0b10, 0b00, 0b11, 0b01][i % 4]);
        }

        let mut phase = 0.0f32;
        let mut iq = Vec::with_capacity(dibits.len() * SPS);
        for dibit in dibits {
            let hz = offset_hz + dibit_level(dibit);
            for _ in 0..SPS {
                phase += TAU * hz / FS as f32;
                iq.push(Complex32::new(phase.cos() * 0.5, phase.sin() * 0.5));
            }
        }
        iq
    }

    /// The point of the per-protocol filters: a strong neighbour in the next
    /// channel is what the inspector's 25 kHz passband used to hand straight
    /// to the NXDN48 receiver, whose signal is only 5 kHz wide.
    #[test]
    fn the_narrow_filter_rejects_an_adjacent_channel_neighbour() {
        let fs = 48_000.0f64;
        let tone = |hz: f64, n: usize| -> Vec<Complex32> {
            let mut phi = 0.0f64;
            (0..n)
                .map(|_| {
                    phi += std::f64::consts::TAU * hz / fs;
                    Complex32::new(phi.cos() as f32, phi.sin() as f32)
                })
                .collect()
        };
        let power = |v: &[Complex32]| -> f64 {
            if v.is_empty() {
                return 0.0;
            }
            f64::from(v.iter().map(|c| c.norm_sqr()).sum::<f32>()) / v.len() as f64
        };

        let mut filters = ChannelFilters::new(fs);
        // On channel, and one 12.5 kHz channel away.
        let wanted = tone(1_000.0, 48_000);
        filters.process(&wanted);
        let kept = power(&filters.narrow_buf);

        let mut filters = ChannelFilters::new(fs);
        let neighbour = tone(12_500.0, 48_000);
        filters.process(&neighbour);
        let rejected = power(&filters.narrow_buf);

        let rejection_db = 10.0 * (kept / rejected.max(1e-18)).log10();
        assert!(
            rejection_db > 40.0,
            "adjacent channel only {rejection_db:.1} dB down"
        );
    }

    /// And it must not have narrowed so far that it damages what it carries.
    #[test]
    fn the_matched_filters_pass_their_own_protocols() {
        let fs = 48_000.0f64;
        let mut filters = ChannelFilters::new(fs);
        let mut phi = 0.0f64;
        // 1.05 kHz deviation is NXDN48's outer symbol; 1.8 kHz is P25's.
        let iq: Vec<Complex32> = (0..48_000)
            .map(|i| {
                let dev = if (i / 20) % 2 == 0 { 1_800.0 } else { -1_800.0 };
                phi += std::f64::consts::TAU * dev / fs;
                Complex32::new(phi.cos() as f32, phi.sin() as f32)
            })
            .collect();
        filters.process(&iq);
        let power = |v: &[Complex32]| -> f64 {
            f64::from(v.iter().map(|c| c.norm_sqr()).sum::<f32>()) / v.len().max(1) as f64
        };
        assert!(
            power(&filters.narrow_buf) > 0.5,
            "narrow filter swallowed a 1.8 kHz-deviation signal"
        );
        assert!(
            power(&filters.standard_buf) > 0.8,
            "standard filter swallowed a 1.8 kHz-deviation signal"
        );
    }

    #[test]
    fn idle_noise_returns_inactive() {
        let mut classifier = SignalClassifier::new(FS);
        let iq: Vec<Complex32> = (0..4800).map(|_| Complex32::new(0.0, 0.0)).collect();
        let res = classifier.process(&iq, 155.0e6);
        assert!(!res.active);
        assert_eq!(res.protocol, "Idle / Noise");
    }

    #[test]
    fn cw_carrier_is_classified() {
        let mut classifier = SignalClassifier::new(FS);
        let iq: Vec<Complex32> = (0..4800)
            .map(|i| {
                let phase = (i as f32 * 200.0 / FS as f32) * TAU;
                Complex32::new(phase.cos() * 0.5, phase.sin() * 0.5)
            })
            .collect();
        let res = classifier.process(&iq, 155.0e6);
        assert!(res.active);
        assert_eq!(res.protocol, "Carrier / Beacon");
    }

    #[test]
    fn rolling_windows_find_4800_baud_four_level_digital() {
        let mut classifier = SignalClassifier::new(FS);
        let iq = fsk_iq(4800, &[-1800.0, -600.0, 600.0, 1800.0], 1200, 1350.0);
        let mut result = ClassificationResult::default();
        // 384 samples approximates a decimated 16k RTL block at 2.048 MS/s and
        // is shorter than many sync/frame observations on its own.
        for block in iq.chunks(384) {
            result = classifier.process(block, 155.0e6);
        }
        assert_eq!(result.modulation, "4-FSK @ 4800 Bd");
        assert!(result.protocol.contains("4-FSK"), "{result:?}");
        assert!(result.confidence >= 0.5);
    }

    #[test]
    fn classifies_offset_9600_baud_binary_fsk() {
        let mut classifier = SignalClassifier::new(FS);
        let iq = fsk_iq(9600, &[-2200.0, 2200.0], 2400, -1700.0);
        let mut result = ClassificationResult::default();
        for block in iq.chunks(480) {
            result = classifier.process(block, 150.0e6);
        }
        assert_eq!(result.modulation, "2-FSK / GMSK @ 9600 Bd");
        assert_eq!(result.protocol, "9600-baud Packet / Telemetry");
    }

    #[test]
    fn weather_frequency_alone_is_not_a_same_alert() {
        let mut classifier = SignalClassifier::new(FS);
        let iq: Vec<Complex32> = (0..12_000)
            .map(|i| {
                let phase = (i as f32 * 300.0 / FS as f32) * TAU;
                Complex32::new(phase.cos() * 0.5, phase.sin() * 0.5)
            })
            .collect();
        let result = classifier.process(&iq, 162.55e6);
        assert_ne!(result.protocol, "NOAA SAME Weather");
    }

    #[test]
    fn wide_discriminator_noise_is_not_digital_traffic() {
        let mut classifier = SignalClassifier::new(FS);
        let mut state = 0x1234_5678u32;
        let iq: Vec<Complex32> = (0..24_000)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let re = ((state >> 16) as i16 as f32) / i16::MAX as f32;
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let im = ((state >> 16) as i16 as f32) / i16::MAX as f32;
                Complex32::new(re * 0.35, im * 0.35)
            })
            .collect();
        let mut result = ClassificationResult::default();
        for block in iq.chunks(384) {
            result = classifier.process(block, 144.825e6);
        }
        assert!(!result.active, "{result:?}");
        assert_eq!(result.protocol, "Idle / Noise");
    }

    #[test]
    fn bell_202_is_identified_across_short_blocks() {
        let mut classifier = SignalClassifier::new(FS);
        let iq = afsk_iq(600);
        let mut result = ClassificationResult::default();
        for block in iq.chunks(384) {
            result = classifier.process(block, 144.39e6);
        }
        assert_eq!(result.protocol, "APRS / AX.25 (probable)", "{result:?}");
    }

    #[test]
    fn production_p25_detector_spans_classifier_blocks() {
        let mut classifier = SignalClassifier::new(FS);
        let iq = p25_frame_iq(0x293, 0x5, 1250.0);
        let mut result = ClassificationResult::default();
        for block in iq.chunks(384) {
            result = classifier.process(block, 773.95625e6);
        }
        assert_eq!(result.protocol, "P25 Phase 1", "{result:?}");
        assert!(
            result
                .details
                .as_deref()
                .unwrap_or_default()
                .contains("NAC: $293")
        );
        assert!(
            result
                .details
                .as_deref()
                .unwrap_or_default()
                .contains("LDU1")
        );
    }

    #[test]
    fn repeated_p25_sync_keeps_ownership_while_nid_fec_is_pending() {
        let mut classifier = SignalClassifier::new(FS);
        classifier.last_p25_candidate = Some((0.86, 3, std::time::Instant::now()));
        // An occupied, quiet four-level FM carrier provides the measurements
        // the decision reports; the candidate state represents syncs already
        // recovered from preceding short SDR blocks.
        let iq = fsk_iq(4800, &[-1800.0, -600.0, 600.0, 1800.0], 120, 0.0);
        let result = classifier.process(&iq, 773.95625e6);
        assert_eq!(result.protocol, "P25 Phase 1 (probable)", "{result:?}");
        assert!(
            result
                .details
                .as_deref()
                .unwrap_or_default()
                .contains("3 repeated")
        );
    }

    #[test]
    fn protected_dmr_metadata_still_confirms_real_dmr() {
        use crate::dmr::{SyncKind, encode_cach, encode_data_burst, modulate};

        let mut dibits = Vec::new();
        for slot in [1, 2, 1, 2, 1, 2] {
            dibits.extend(encode_cach(slot));
            dibits.extend(encode_data_burst(7, 9, SyncKind::BsData));
        }
        let iq = modulate(&dibits, FS);
        let mut classifier = SignalClassifier::new(FS);
        let mut result = ClassificationResult::default();
        for block in iq.chunks(384) {
            result = classifier.process(block, 462.2e6);
        }
        assert_eq!(result.protocol, "DMR / MotoTRBO", "{result:?}");
        assert!(
            result
                .details
                .as_deref()
                .unwrap_or_default()
                .contains("Color Code 7")
        );
    }
}
