//! DRM30 (Digital Radio Mondiale, ETSI ES 201 980) for LF, MF and HF:
//! robustness modes A to D, spectrum occupancies 0 to 5, from complex
//! baseband centered on the station's nominal frequency to the codec's
//! access units, the station's label and text, and the time.
//!
//! **The signal.** DRM is OFDM: up to 460 carriers 41.7 to 107 Hz apart,
//! symbols of 16.7 to 26.7 ms each with a cyclic prefix (the guard
//! interval) that soaks up the echoes of sky-wave paths. Fifteen to
//! twenty-four symbols make a 400 ms transmission frame, three frames a
//! super frame. Known pilot cells are scattered over the time-frequency
//! grid for the receiver to measure the channel by; three carriers are
//! continuous tones; the first symbol of each frame has a set of time
//! references. The rest carries three channels: the FAC (fast access, 65
//! cells of 4-QAM per frame: mode, bandwidth, coding, one service's id),
//! the SDC (service description, the first symbols of each super frame:
//! the multiplex, labels, audio coding, time) and the MSC (the services'
//! data, 16- or 64-QAM, interleaved over one or five frames).
//!
//! **Acquisition.** The receiver resamples to exactly 48 kHz. The guard
//! interval repeats the end of each symbol, so the correlation between the
//! signal (band-limited to 8 kHz either side) and itself one useful symbol
//! length later, summed over a guard's length and folded modulo the
//! symbol length over 0.8 s, peaks where the guards are, for the mode
//! whose lengths match; the peak's phase is the frequency offset modulo
//! one carrier spacing. The whole-carrier part of the offset (searched to
//! 2.5 kHz either side) comes from the frequency references: their bins
//! keep the same phase from symbol to symbol while data bins wander, so
//! the shift that makes them coherent is the one. Frames are found from
//! the time references (products of nearby references, grouped by their
//! spacing so the timing's residual error cannot cancel them, summed over
//! frames), the occupancy from where the carriers have power, and both are
//! confirmed by the first FAC whose CRC checks, which also says which
//! frame of the super frame it is.
//!
//! **Tracking.** Each symbol is cut from half way into its guard, and the
//! FFT turned back by the same amount, so echoes either side of the main
//! path fit. The channel is estimated from the gain references (see
//! `chanest`): linear in time, Wiener in frequency over a window fitted to
//! the measured delay profile. The mean delay it measures steers the
//! symbol position (proportionally, fast enough to keep fading echoes
//! inside the guard) and, slowly, the period, which takes up the
//! transmitter's sample clock (±50 ppm tested); each step the loop takes
//! is turned into what the estimator holds, so time interpolation never
//! straddles two timings. The frequency follows the channel's mean turn
//! from symbol to symbol across all pilot carriers; the frequency
//! references alone fall into the notches echoes cut and would also see
//! the timing loop's steps.
//!
//! **Decoding.** Cells are equalized and weighted by their channel power;
//! FAC, SDC and MSC go through the multilevel decoder (`coding`) with soft
//! decisions and Viterbi decoding. The FAC (CRC-8) and SDC (CRC-16) are
//! used only when their CRCs check. The MSC is cell-deinterleaved, decoded
//! with the protection levels the SDC gives, its energy dispersal undone
//! and cut into streams by the SDC's multiplex description; each audio
//! service's stream becomes its text message and its audio super frame,
//! which is cut into the codec's access units (`audio`).
//!
//! **What the AAC decoder gets** ([`DrmEvent::AudioSuperFrame`]): for
//! AAC, each frame is one raw error-robust AAC frame
//! (aac_scalable_main_element of ISO/IEC 14496-3, the 960-sample transform
//! length, so 80 ms at a 12 kHz core and 40 ms at 24 kHz; 5 or 10 to a
//! super frame), with its separate CRC byte. To configure the decoder:
//! audio object type ER AAC LC (17) with frameLengthFlag set (960), the
//! core sampling rate (`sample_rate_hz`, 12 or 24 kHz), mono unless the
//! SDC says stereo; with `sbr` the SBR tool at twice the core rate (the
//! SBR data is at the end of each frame, read backwards, table 12); with
//! `ps` parametric stereo on a mono core. `audio_config` carries the SDC
//! type 9 body (audio coding, SBR, audio mode, sampling rate, text,
//! enhancement, coder field: two bytes) as FDK-AAC's DRM transport takes
//! it. For xHE-AAC each frame is one USAC access unit and `audio_config`
//! carries, after those two bytes, the stream's static configuration
//! (xHEAACStaticConfig, table 4).
//!
//! Services with conditional access on their audio are described but
//! their audio is not handed out.
//!
//! **Not implemented:** robustness mode E (DRM+); the hierarchical MSC
//! modes of earlier versions of the standard (reserved since V4); data
//! services (packet mode, MOT slideshow, Journaline): their streams are
//! decoded but not handed out; SDC types other than 0, 1, 3, 8, 9 and 12
//! (alternative frequencies of other services, type 11, among them); the
//! AFS index; multiplex reconfiguration beyond following the SDC; and
//! iterative multilevel decoding.

mod audio;
mod cellmap;
mod chanest;
mod coding;
mod info;
mod resample;
mod tables;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tx;

pub use audio::{AudioCodec, AudioFrame};
pub use coding::crc8_bits;
pub use tables::Mode;

use audio::{TextAssembler, TextUpdate, XheAssembler};
use cellmap::CellMap;
use chanest::{ChanEst, Equalized};
use coding::{CellDeinterleaver, Mlc, Viterbi};
use info::{AudioInfo, Fac, LanguageCountry, Multiplex, Sdc};
use num_complex::Complex32;
use resample::Resampler;
use rustfft::{Fft, FftPlanner};
use std::f64::consts::TAU;
use std::sync::Arc;
use tables::{FS, MODES};

/// Samples folded per look for the guard correlation (0.8 s).
const SEARCH_BLOCK: usize = 38_400;
/// Looks folded together before starting over.
const SEARCH_LOOKS: usize = 3;
/// The folded guard correlation's peak must stand this far above its
/// median. Noise folds to a few hundredths at most, a tone (an AM
/// carrier) to a flat line; a DRM signal whose FAC can decode (4 dB in
/// its band, noise filling the search filter's band) to about 0.6.
const SEARCH_CONTRAST: f32 = 0.2;
/// The search looks only within this much of the nominal frequency, so
/// that noise and other stations across the receiver's 48 kHz do not
/// dilute the correlation: 10 kHz occupancies with 2.5 kHz of offset, and
/// enough of the wider ones.
const SEARCH_BAND_HZ: f64 = 8_000.0;
const SEARCH_TAPS: usize = 49;
/// Integer frequency search span, Hz either side.
const INT_SPAN_HZ: f64 = 2_500.0;
/// Symbol pairs for the integer frequency search.
const INT_PAIRS: usize = 12;
/// Frequency loop gain per symbol.
const FREQ_GAIN: f64 = 0.03;
/// Timing loop: position and period gains per symbol, on the mean delay.
/// The period follows only slowly: the mean delay also moves as echoes
/// fade against each other, which is not a clock error. Proportional
/// control alone leaves a 50 ppm clock error about a sample off.
const TIME_KP: f64 = 0.05;
const TIME_KI: f64 = 0.00005;
/// Consecutive FAC failures before the lock counts as lost, once it has
/// been confirmed, and before a lock never confirmed is given up.
const FAC_LOST: u32 = 12;
const FAC_NEVER: u32 = 6;
/// Blocks the CRC success rates are taken over.
const RATE_WINDOW: usize = 20;
/// Cells kept for the constellation view.
const CONSTELLATION_CELLS: usize = 1024;

/// What an MSC coding depends on: cells, levels, protection levels and
/// part A's bytes.
type MscKey = (usize, usize, u8, u8, usize);

/// One service as the FAC and SDC describe it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DrmService {
    pub short_id: u8,
    pub service_id: u32,
    pub label: Option<String>,
    pub audio: bool,
    /// The FAC's language (table 18).
    pub language: Option<&'static str>,
    /// ISO 639-2 language and ISO 3166 country codes from SDC type 12.
    pub language_code: Option<String>,
    pub country_code: Option<String>,
    /// Program type (audio services, table 19).
    pub program_type: Option<&'static str>,
    pub codec: Option<AudioCodec>,
    /// Core coder sampling rate, Hz.
    pub sample_rate_hz: Option<u32>,
    pub sbr: bool,
    /// "mono", "parametric stereo" or "stereo".
    pub audio_mode: Option<&'static str>,
    pub text: bool,
    /// Conditional access on the audio or the data.
    pub ca: bool,
}

/// Counters behind the CRC rates, per Stats event.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DrmStats {
    pub fac_ok: u64,
    pub fac_bad: u64,
    pub sdc_ok: u64,
    pub sdc_bad: u64,
    /// Audio super frames whose frames all passed the checks this module
    /// can make (see [`AudioFrame::ok`]), and those that did not.
    pub msc_ok: u64,
    pub msc_bad: u64,
    pub mer_db: Option<f32>,
    pub wmer_db: Option<f32>,
    /// Raw (pre-Viterbi) bit error rate of the MSC, measured against the
    /// decoded and re-encoded code word.
    pub msc_ber: Option<f32>,
    pub freq_offset_hz: f32,
    pub clock_ppm: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DrmEvent {
    /// Lock confirmed by an FAC (`locked`, with mode and occupancy), or lost.
    Sync { locked: bool, mode: Option<char>, so: Option<u8> },
    /// A service's description changed.
    Service(DrmService),
    /// A text message completed (an empty `text` is the clear command).
    Text { short_id: u8, service_id: u32, text: String },
    /// SDC type 8.
    Time { year: i32, month: u8, day: u8, hour: u8, minute: u8, local_offset_min: Option<i32> },
    /// One audio super frame (400 ms) of a service, cut into access units.
    AudioSuperFrame {
        service: u8,
        codec: AudioCodec,
        /// Core coder rate, Hz; with `sbr` the output is twice this.
        sample_rate_hz: u32,
        sbr: bool,
        stereo: bool,
        ps: bool,
        /// SDC type 9 from the audio coding field on (see the module doc).
        audio_config: Vec<u8>,
        frames: Vec<AudioFrame>,
    },
    /// Once per super frame while locked.
    Stats(DrmStats),
}

impl DrmEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            DrmEvent::Sync { .. } => "Sync",
            DrmEvent::Service(_) => "Service",
            DrmEvent::Text { .. } => "Text",
            DrmEvent::Time { .. } => "Time",
            DrmEvent::AudioSuperFrame { .. } => "AudioSuperFrame",
            DrmEvent::Stats(_) => "Stats",
        }
    }

    pub fn summary(&self) -> String {
        match self {
            DrmEvent::Sync { locked: true, mode, so } => format!(
                "DRM locked: mode {}, {} kHz",
                mode.map_or('?', |m| m),
                so.map_or("?".to_string(), |s| format!("{}", tables::bandwidth_hz(s) / 1000.0))
            ),
            DrmEvent::Sync { .. } => "DRM lost".to_string(),
            DrmEvent::Service(s) => {
                let mut t = s.label.clone().unwrap_or_else(|| format!("{:06X}", s.service_id));
                if let Some(c) = s.codec {
                    t += &format!(" ({}", c.name());
                    if s.sbr {
                        t += "+SBR";
                    }
                    if let Some(m) = s.audio_mode {
                        t += &format!(", {m}");
                    }
                    t += ")";
                }
                if let Some(p) = s.program_type {
                    t += &format!(", {p}");
                }
                t
            }
            DrmEvent::Text { text, .. } => text.clone(),
            DrmEvent::Time { year, month, day, hour, minute, .. } => {
                format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02} UTC")
            }
            DrmEvent::AudioSuperFrame { codec, frames, .. } => {
                let ok = frames.iter().filter(|f| f.ok).count();
                format!("{} super frame, {}/{} frames ok", codec.name(), ok, frames.len())
            }
            DrmEvent::Stats(s) => format!(
                "FAC {}/{}, SDC {}/{}, MER {}",
                s.fac_ok,
                s.fac_ok + s.fac_bad,
                s.sdc_ok,
                s.sdc_ok + s.sdc_bad,
                s.mer_db.map_or("-".to_string(), |m| format!("{m:.1} dB"))
            ),
        }
    }

    pub fn fields(&self) -> Vec<(&'static str, String)> {
        let opt = |v: Option<String>| v.unwrap_or_default();
        match self {
            DrmEvent::Sync { locked, mode, so } => vec![
                ("locked", locked.to_string()),
                ("mode", opt(mode.map(|m| m.to_string()))),
                ("spectrumOccupancy", opt(so.map(|s| s.to_string()))),
                ("bandwidthHz", opt(so.map(|s| format!("{}", tables::bandwidth_hz(s))))),
            ],
            DrmEvent::Service(s) => vec![
                ("shortId", s.short_id.to_string()),
                ("serviceId", format!("{:06X}", s.service_id)),
                ("label", opt(s.label.clone())),
                ("audio", s.audio.to_string()),
                ("language", opt(s.language.map(str::to_string))),
                ("languageCode", opt(s.language_code.clone())),
                ("countryCode", opt(s.country_code.clone())),
                ("programType", opt(s.program_type.map(str::to_string))),
                ("codec", opt(s.codec.map(|c| c.name().to_string()))),
                ("sampleRateHz", opt(s.sample_rate_hz.map(|r| r.to_string()))),
                ("sbr", s.sbr.to_string()),
                ("audioMode", opt(s.audio_mode.map(str::to_string))),
                ("text", s.text.to_string()),
                ("ca", s.ca.to_string()),
            ],
            DrmEvent::Text { short_id, service_id, text } => vec![
                ("shortId", short_id.to_string()),
                ("serviceId", format!("{service_id:06X}")),
                ("text", text.clone()),
            ],
            DrmEvent::Time { year, month, day, hour, minute, local_offset_min } => vec![
                ("date", format!("{year:04}-{month:02}-{day:02}")),
                ("utc", format!("{hour:02}:{minute:02}")),
                ("localOffsetMin", opt(local_offset_min.map(|o| o.to_string()))),
            ],
            DrmEvent::AudioSuperFrame { service, codec, sample_rate_hz, sbr, stereo, ps, frames, .. } => vec![
                ("service", service.to_string()),
                ("codec", codec.name().to_string()),
                ("sampleRateHz", sample_rate_hz.to_string()),
                ("sbr", sbr.to_string()),
                ("stereo", stereo.to_string()),
                ("ps", ps.to_string()),
                ("frames", frames.len().to_string()),
                ("framesOk", frames.iter().filter(|f| f.ok).count().to_string()),
                ("bytes", frames.iter().map(|f| f.data.len()).sum::<usize>().to_string()),
            ],
            DrmEvent::Stats(s) => vec![
                ("facOk", s.fac_ok.to_string()),
                ("facBad", s.fac_bad.to_string()),
                ("sdcOk", s.sdc_ok.to_string()),
                ("sdcBad", s.sdc_bad.to_string()),
                ("mscOk", s.msc_ok.to_string()),
                ("mscBad", s.msc_bad.to_string()),
                ("merDb", opt(s.mer_db.map(|v| format!("{v:.1}")))),
                ("wmerDb", opt(s.wmer_db.map(|v| format!("{v:.1}")))),
                ("mscBer", opt(s.msc_ber.map(|v| format!("{v:.4}")))),
                ("freqOffsetHz", format!("{:.2}", s.freq_offset_hz)),
                ("clockPpm", format!("{:.1}", s.clock_ppm)),
            ],
        }
    }
}

/// The receiver's state for a status display.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DrmStatus {
    /// Guard correlation found a mode and symbol timing.
    pub time_locked: bool,
    /// The whole-carrier frequency offset is resolved.
    pub freq_locked: bool,
    /// Frames found from the time references.
    pub frame_locked: bool,
    /// The last FACs passed their CRC (more than half of recent ones).
    pub fac_locked: bool,
    pub sdc_locked: bool,
    /// Audio super frames are coming out.
    pub msc_locked: bool,
    pub mode: Option<char>,
    pub spectrum_occupancy: Option<u8>,
    pub bandwidth_hz: Option<f32>,
    pub msc_qam: Option<u8>,
    pub sdc_qam: Option<u8>,
    /// Protection levels of parts A and B.
    pub protection: Option<(u8, u8)>,
    pub long_interleave: Option<bool>,
    /// Signal to noise ratio of a data cell, from the pilots: the scatter
    /// of each symbol's gain references about the smoothed channel
    /// estimate (pilots have twice a data cell's power).
    pub snr_db: Option<f32>,
    /// MER of the MSC cells (decision directed) and the same weighted by
    /// each cell's channel power.
    pub mer_db: Option<f32>,
    pub wmer_db: Option<f32>,
    /// Frequency offset of the signal from the nominal frequency, Hz.
    pub freq_offset_hz: f32,
    /// The transmitter's sample clock against the receiver's, ppm
    /// (positive: the transmitter's runs fast).
    pub clock_ppm: f32,
    /// The rms delay spread of the averaged power delay profile (its
    /// noise floor taken out), ms. Close for two paths in modes A to C;
    /// in mode D with long, fast-fading echoes it reads low.
    pub delay_spread_ms: Option<f32>,
    /// The Doppler (the maximum of a classic Rayleigh spectrum), Hz, from
    /// the correlation between one carrier's successive pilots, corrected
    /// for the pilots' noise. Rough: about a few tenths of a hertz too high
    /// below 10 dB SNR.
    pub doppler_hz: Option<f32>,
    pub fac_ok_rate: f32,
    pub sdc_ok_rate: f32,
    pub msc_ok_rate: f32,
    pub labels: Vec<String>,
    pub services: Vec<DrmService>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Search,
    FreqInt,
    FrameSync,
    Locked,
}

/// One transmission frame's equalized cells, by frame symbol.
struct FrameBuf {
    z: Vec<Vec<Complex32>>,
    w: Vec<Vec<f32>>,
    next: usize,
}

/// A super frame's MSC cells being gathered.
#[derive(Default)]
struct SuperFrame {
    z: Vec<Complex32>,
    w: Vec<f32>,
    /// The frame expected next, while gathering.
    next: Option<u8>,
}

#[derive(Default, Clone)]
struct ServiceState {
    fac: Option<info::FacService>,
    label: Option<String>,
    lang: Option<LanguageCountry>,
    audio: Option<AudioInfo>,
    reported: Option<DrmService>,
}

/// Success rate over the last few blocks.
#[derive(Default)]
struct Rate(std::collections::VecDeque<bool>);

impl Rate {
    fn add(&mut self, ok: bool) {
        if self.0.len() == RATE_WINDOW {
            self.0.pop_front();
        }
        self.0.push_back(ok);
    }

    fn get(&self) -> f32 {
        if self.0.is_empty() { 0.0 } else { self.0.iter().filter(|&&b| b).count() as f32 / self.0.len() as f32 }
    }
}

#[derive(Default)]
struct Mer {
    sig: f64,
    err: f64,
    wsig: f64,
    werr: f64,
}

impl Mer {
    fn add(&mut self, z: &[Complex32], w: &[f32], levels: usize) {
        let (mut s, mut e, mut ws, mut we) = (0.0, 0.0, 0.0, 0.0);
        for (&z, &w) in z.iter().zip(w.iter()) {
            if w <= 0.0 {
                continue;
            }
            let d = coding::slice(z, levels);
            let ss = d.norm_sqr() as f64;
            let ee = (z - d).norm_sqr() as f64;
            s += ss;
            e += ee;
            ws += w as f64 * ss;
            we += w as f64 * ee;
        }
        let a = 0.3;
        self.sig = (1.0 - a) * self.sig + a * s;
        self.err = (1.0 - a) * self.err + a * e;
        self.wsig = (1.0 - a) * self.wsig + a * ws;
        self.werr = (1.0 - a) * self.werr + a * we;
    }

    fn db(&self) -> Option<(f32, f32)> {
        if self.sig <= 0.0 || self.err <= 0.0 {
            return None;
        }
        Some((
            (10.0 * (self.sig / self.err).log10()) as f32,
            (10.0 * (self.wsig / self.werr.max(1e-30)).log10()) as f32,
        ))
    }
}

pub struct DrmReceiver {
    resampler: Resampler,
    rs_out: Vec<Complex32>,
    buf: Vec<Complex32>,
    /// Absolute sample index (48 kHz) of `buf[0]`.
    base: u64,
    /// Frequency correction applied when samples are read: the phase of
    /// the correction at sample `nco_n`, and its frequency.
    nco_hz: f64,
    nco_n: u64,
    nco_phase: f64,
    ffts: Vec<Arc<dyn Fft<f32>>>,
    phase: Phase,
    // Search.
    search_at: u64,
    looks: usize,
    fold: Vec<Vec<Complex32>>,
    fold_e: Vec<Vec<f32>>,
    search_fir: Vec<f32>,
    // Tracking.
    mode: Mode,
    /// Absolute position of the next symbol's useful part (where the FFT
    /// would start for a path with no delay), and the symbol period.
    sym_pos: f64,
    period: f64,
    /// A least squares line through the symbol positions (less the
    /// nominal period) against the symbol count since lock: its slope is
    /// the clock offset, the mean delay's wandering averaged out.
    /// [n, sum x, sum y, sum x^2, sum x y], and the first position.
    clock_fit: [f64; 5],
    clock_origin: (u64, f64),
    count: u64,
    phase_count: u64,
    prev: Vec<Complex32>,
    int_acc: Vec<[(Complex32, f32); 3]>,
    int_pow: f32,
    tref: Vec<([Complex32; 5], f32)>,
    power: Vec<f32>,
    frame_offset: u64,
    map: Option<CellMap>,
    est: Option<ChanEst>,
    frame: Option<FrameBuf>,
    // Decoding.
    vit: Viterbi,
    fac_mlc: Mlc,
    sdc_mlc: Option<(usize, bool, Mlc)>,
    msc_mlc: Option<(MscKey, Mlc)>,
    fac: Option<Fac>,
    fac_confirmed: bool,
    fac_fail: u32,
    frame_index: Option<u8>,
    multiplex: Option<Multiplex>,
    services: [ServiceState; 4],
    time: Option<info::TimeDate>,
    sf: SuperFrame,
    deint: Option<CellDeinterleaver>,
    text: [TextAssembler; 4],
    xhe: [XheAssembler; 4],
    stats: DrmStats,
    fac_rate: Rate,
    sdc_rate: Rate,
    msc_rate: Rate,
    sdc_seen: bool,
    msc_seen: bool,
    fac_mer: Mer,
    msc_mer: Mer,
    constellation: Vec<Complex32>,
    events: Vec<DrmEvent>,
}

/// A low-pass of +/-`SEARCH_BAND_HZ` at 48 kHz: a Hamming-windowed sinc.
fn search_filter() -> Vec<f32> {
    let fc = SEARCH_BAND_HZ / FS;
    let mid = (SEARCH_TAPS - 1) as f64 / 2.0;
    let taps: Vec<f64> = (0..SEARCH_TAPS)
        .map(|i| {
            let t = i as f64 - mid;
            let sinc = if t == 0.0 { 2.0 * fc } else { (TAU * fc * t).sin() / (std::f64::consts::PI * t) };
            sinc * (0.54 - 0.46 * (TAU * i as f64 / (SEARCH_TAPS - 1) as f64).cos())
        })
        .collect();
    let sum: f64 = taps.iter().sum();
    taps.iter().map(|t| (t / sum) as f32).collect()
}

fn carrier_bin(k: i32, nu: usize) -> usize {
    k.rem_euclid(nu as i32) as usize
}

impl DrmReceiver {
    pub fn new(fs: f32) -> Self {
        let mut planner = FftPlanner::new();
        let ffts = MODES.iter().map(|m| planner.plan_fft_forward(m.tu())).collect();
        DrmReceiver {
            resampler: Resampler::new(fs as f64, FS, 16),
            rs_out: Vec::new(),
            buf: Vec::new(),
            base: 0,
            nco_hz: 0.0,
            nco_n: 0,
            nco_phase: 0.0,
            ffts,
            phase: Phase::Search,
            search_at: 0,
            looks: 0,
            fold: MODES.iter().map(|m| vec![Complex32::new(0.0, 0.0); m.ts()]).collect(),
            fold_e: MODES.iter().map(|m| vec![0.0; m.ts()]).collect(),
            search_fir: search_filter(),
            mode: Mode::B,
            sym_pos: 0.0,
            period: 0.0,
            clock_fit: [0.0; 5],
            clock_origin: (0, 0.0),
            count: 0,
            phase_count: 0,
            prev: Vec::new(),
            int_acc: Vec::new(),
            int_pow: 0.0,
            tref: Vec::new(),
            power: Vec::new(),
            frame_offset: 0,
            map: None,
            est: None,
            frame: None,
            vit: Viterbi::new(),
            fac_mlc: Mlc::fac(),
            sdc_mlc: None,
            msc_mlc: None,
            fac: None,
            fac_confirmed: false,
            fac_fail: 0,
            frame_index: None,
            multiplex: None,
            services: Default::default(),
            time: None,
            sf: SuperFrame::default(),
            deint: None,
            text: Default::default(),
            xhe: Default::default(),
            stats: DrmStats::default(),
            fac_rate: Rate::default(),
            sdc_rate: Rate::default(),
            msc_rate: Rate::default(),
            sdc_seen: false,
            msc_seen: false,
            fac_mer: Mer::default(),
            msc_mer: Mer::default(),
            constellation: Vec::new(),
            events: Vec::new(),
        }
    }

    /// Feed complex baseband centered on the DRM channel's nominal
    /// frequency.
    pub fn push_iq(&mut self, iq: &[Complex32]) {
        self.rs_out.clear();
        self.resampler.process(iq, &mut self.rs_out);
        self.buf.extend_from_slice(&self.rs_out);
        while self.step() {}
        // Drop what is no longer needed.
        let keep = match self.phase {
            Phase::Search => self.search_at,
            _ => (self.sym_pos - self.mode.ts() as f64).max(0.0) as u64,
        };
        if keep > self.base + 4096 {
            let n = ((keep - self.base) as usize).min(self.buf.len());
            self.buf.drain(..n);
            self.base += n as u64;
        }
    }

    pub fn take(&mut self) -> Vec<DrmEvent> {
        std::mem::take(&mut self.events)
    }

    /// The last cells of the MSC after equalization, for a constellation
    /// view.
    pub fn constellation(&self) -> Vec<Complex32> {
        self.constellation.clone()
    }

    pub fn status(&self) -> DrmStatus {
        let locked = self.phase != Phase::Search;
        let (mer, wmer) = self.msc_mer.db().map_or((None, None), |(a, b)| (Some(a), Some(b)));
        let fac_snr = self.fac_mer.db().map(|v| v.1);
        let map = self.map.as_ref();
        let fac = self.fac.as_ref().filter(|_| self.fac_confirmed);
        let mut services = Vec::new();
        let mut labels = Vec::new();
        for s in self.services.iter() {
            if let Some(r) = &s.reported {
                if let Some(l) = &r.label {
                    labels.push(l.clone());
                }
                services.push(r.clone());
            }
        }
        let delay_spread_ms =
            self.est.as_ref().filter(|_| self.phase == Phase::Locked).map(|e| e.spread / FS as f32 * 1000.0);
        // Jakes: the correlation y symbols apart is J0(2 pi fd dt), about
        // 1 - (pi fd dt)^2; noise on the pilots lowers it by 1 / (1 + NSR).
        let doppler_hz = self.est.as_ref().filter(|_| self.phase == Phase::Locked).map(|e| {
            let rho = (e.trho * (1.0 + e.pilot_nsr)).min(1.0);
            let (_, y, _) = self.mode.grid();
            let dt = y as f32 * self.mode.ts() as f32 / FS as f32;
            (1.0 - rho).sqrt() / (std::f32::consts::PI * dt)
        });
        DrmStatus {
            time_locked: locked,
            freq_locked: matches!(self.phase, Phase::FrameSync | Phase::Locked),
            frame_locked: self.phase == Phase::Locked,
            fac_locked: self.phase == Phase::Locked && self.fac_rate.get() > 0.5,
            sdc_locked: self.sdc_seen && self.sdc_rate.get() > 0.5,
            msc_locked: self.msc_seen && self.msc_rate.get() > 0.5,
            mode: locked.then(|| self.mode.letter()),
            spectrum_occupancy: map.map(|m| m.so),
            bandwidth_hz: map.map(|m| tables::bandwidth_hz(m.so) as f32),
            msc_qam: fac.and_then(|f| match f.msc_mode {
                0 => Some(64),
                3 => Some(16),
                _ => None,
            }),
            sdc_qam: fac.map(|f| if f.sdc_qam16 { 16 } else { 4 }),
            protection: self.multiplex.as_ref().map(|m| (m.pl_a, m.pl_b)),
            long_interleave: fac.map(|f| f.long_interleave),
            snr_db: self
                .est
                .as_ref()
                .filter(|e| self.phase == Phase::Locked && e.pilot_nsr > 0.0)
                .map(|e| -10.0 * (2.0 * e.pilot_nsr).log10())
                .or(fac_snr),
            mer_db: mer,
            wmer_db: wmer,
            freq_offset_hz: if locked { self.nco_hz as f32 } else { 0.0 },
            clock_ppm: self.clock_ppm(),
            delay_spread_ms,
            doppler_hz,
            fac_ok_rate: self.fac_rate.get(),
            sdc_ok_rate: self.sdc_rate.get(),
            msc_ok_rate: self.msc_rate.get(),
            labels,
            services,
        }
    }

    fn clock_ppm(&self) -> f32 {
        let [n, sx, sy, sxx, sxy] = self.clock_fit;
        let den = n * sxx - sx * sx;
        if self.phase != Phase::Locked || n < 100.0 || den <= 0.0 {
            return 0.0;
        }
        // Symbols arriving early (a negative slope) are a fast clock.
        let slope = (n * sxy - sx * sy) / den;
        (-slope / self.mode.ts() as f64 * 1e6) as f32
    }

    fn avail(&self) -> u64 {
        self.base + self.buf.len() as u64
    }

    /// One unit of work; false when more samples are needed.
    fn step(&mut self) -> bool {
        match self.phase {
            Phase::Search => self.search(),
            _ => self.track(),
        }
    }

    // -----------------------------------------------------------------
    // Acquisition: mode and timing from the guard correlation.

    fn search(&mut self) -> bool {
        let start = self.search_at.max(self.base);
        let len = SEARCH_BLOCK + 1152 + 352;
        let need = start + (len + SEARCH_TAPS) as u64;
        if self.avail() < need {
            return false;
        }
        let off = (start - self.base) as usize;
        // Band-limited; r[n] is the filter's output centered on sample
        // start + n (its delay taken out).
        let src = &self.buf[off..off + len + SEARCH_TAPS];
        let r: Vec<Complex32> = (0..len)
            .map(|n| src[n..n + SEARCH_TAPS].iter().zip(&self.search_fir).map(|(x, h)| x * h).sum::<Complex32>())
            .collect();
        let start = start + (SEARCH_TAPS as u64 - 1) / 2;
        for (mi, m) in MODES.iter().enumerate() {
            let (nu, ng, ns) = (m.tu(), m.tg(), m.ts());
            // Prefix sums of r[n] r*[n + nu] and of the power.
            let len = SEARCH_BLOCK + ng;
            let mut cs = Vec::with_capacity(len + 1);
            let mut es = Vec::with_capacity(len + 1);
            cs.push(Complex32::new(0.0, 0.0));
            es.push(0.0f32);
            let (mut c, mut e) = (Complex32::new(0.0, 0.0), 0.0f32);
            for n in 0..len {
                c += r[n] * r[n + nu].conj();
                e += 0.5 * (r[n].norm_sqr() + r[n + nu].norm_sqr());
                cs.push(c);
                es.push(e);
            }
            let fold = &mut self.fold[mi];
            let fold_e = &mut self.fold_e[mi];
            for n in 0..SEARCH_BLOCK {
                let slot = ((start + n as u64) % ns as u64) as usize;
                fold[slot] += cs[n + ng] - cs[n];
                fold_e[slot] += es[n + ng] - es[n];
            }
        }
        self.looks += 1;
        // The best mode: highest peak over the folded profile's median.
        let mut best: Option<(usize, f32, usize)> = None;
        for (mi, _) in MODES.iter().enumerate() {
            let prof: Vec<f32> = self.fold[mi]
                .iter()
                .zip(self.fold_e[mi].iter())
                .map(|(c, &e)| if e > 0.0 { c.norm() / e } else { 0.0 })
                .collect();
            let mut sorted = prof.clone();
            sorted.sort_by(|a, b| a.total_cmp(b));
            let median = sorted[sorted.len() / 2];
            let (pk, &pv) = prof.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap_or((0, &0.0));
            let contrast = pv - median;
            if best.is_none_or(|b| contrast > b.1) {
                best = Some((mi, contrast, pk));
            }
        }
        if let Some((mi, contrast, pk)) = best
            && contrast > SEARCH_CONTRAST
        {
            self.start_tracking(MODES[mi], pk, start);
            return true;
        }
        if self.looks >= SEARCH_LOOKS {
            self.reset_search_folds();
        }
        self.search_at = start - (SEARCH_TAPS as u64 - 1) / 2 + SEARCH_BLOCK as u64;
        true
    }

    fn reset_search_folds(&mut self) {
        self.looks = 0;
        for f in self.fold.iter_mut() {
            f.iter_mut().for_each(|c| *c = Complex32::new(0.0, 0.0));
        }
        for f in self.fold_e.iter_mut() {
            f.iter_mut().for_each(|c| *c = 0.0);
        }
    }

    fn start_tracking(&mut self, mode: Mode, peak: usize, block_start: u64) {
        let mi = mode.index();
        let ns = mode.ts();
        let prof: Vec<f32> = self.fold[mi]
            .iter()
            .zip(self.fold_e[mi].iter())
            .map(|(c, &e)| if e > 0.0 { c.norm() / e } else { 0.0 })
            .collect();
        // Echoes widen the peak into a plateau; take its middle.
        let pv = prof[peak];
        let mut lo = 0;
        while lo < ns / 2 && prof[(peak + ns - lo - 1) % ns] > 0.7 * pv {
            lo += 1;
        }
        let mut hi = 0;
        while hi < ns / 2 && prof[(peak + hi + 1) % ns] > 0.7 * pv {
            hi += 1;
        }
        let center = (peak as isize + (hi as isize - lo as isize).div_euclid(2)).rem_euclid(ns as isize) as usize;
        let c = self.fold[mi][center];
        // r[n] r*[n + Tu] turns by -2 pi f Tu / fs.
        let frac_hz = -(c.arg() as f64) * FS / (TAU * mode.tu() as f64);
        self.reset_search_folds();
        self.mode = mode;
        self.set_nco(frac_hz, block_start);
        let useful = (center + mode.tg()) % ns;
        let r = (useful as i64 - (block_start % ns as u64) as i64).rem_euclid(ns as i64) as u64;
        self.sym_pos = (block_start + r + ns as u64) as f64;
        self.period = ns as f64;
        self.clock_fit = [0.0; 5];
        self.phase = Phase::FreqInt;
        self.phase_count = 0;
        self.count = 0;
        self.prev.clear();
        let q = (INT_SPAN_HZ / mode.spacing_hz()) as usize;
        self.int_acc = vec![[(Complex32::new(0.0, 0.0), 0.0); 3]; 2 * q + 1];
        self.int_pow = 0.0;
    }

    fn set_nco(&mut self, hz: f64, at: u64) {
        // Keep the correction's phase continuous at `at`.
        self.nco_phase = self.nco_phase_at(at);
        self.nco_n = at;
        self.nco_hz = hz;
    }

    fn nco_phase_at(&self, n: u64) -> f64 {
        let d = n as f64 - self.nco_n as f64;
        (self.nco_phase + TAU * self.nco_hz * d / FS).rem_euclid(TAU)
    }

    fn lose(&mut self) {
        if self.fac_confirmed {
            self.events.push(DrmEvent::Sync { locked: false, mode: None, so: None });
        }
        self.phase = Phase::Search;
        self.search_at = self.avail().saturating_sub(SEARCH_BLOCK as u64 / 2);
        self.reset_search_folds();
        self.map = None;
        self.est = None;
        self.frame = None;
        self.fac = None;
        self.fac_confirmed = false;
        self.fac_fail = 0;
        self.frame_index = None;
        self.multiplex = None;
        self.services = Default::default();
        self.sf = SuperFrame::default();
        self.deint = None;
        self.text = Default::default();
        self.xhe = Default::default();
        self.sdc_seen = false;
        self.msc_seen = false;
        self.fac_rate = Rate::default();
        self.sdc_rate = Rate::default();
        self.msc_rate = Rate::default();
        self.time = None;
        self.fac_mer = Mer::default();
        self.msc_mer = Mer::default();
        self.nco_hz = 0.0;
    }

    // -----------------------------------------------------------------
    // Tracking.

    /// FFT of the symbol whose useful part starts at `pos`, cut from half
    /// way into its guard and turned back by as much (and by the fraction
    /// of a sample `pos` falls between samples). `None` until the samples
    /// are in.
    fn fft_at(&mut self, pos: f64) -> Option<Vec<Complex32>> {
        let (nu, ng) = (self.mode.tu(), self.mode.tg());
        let start = (pos - ng as f64 / 2.0).floor();
        if start < self.base as f64 {
            return Some(vec![Complex32::new(0.0, 0.0); nu]);
        }
        let wi = start as u64;
        if wi + nu as u64 > self.avail() {
            return None;
        }
        let d = pos - wi as f64;
        let off = (wi - self.base) as usize;
        let ph0 = self.nco_phase_at(wi);
        let dph = -TAU * self.nco_hz / FS;
        let mut rot = Complex32::from_polar(1.0, -ph0 as f32);
        let step = Complex32::from_polar(1.0, dph as f32);
        let mut v: Vec<Complex32> = Vec::with_capacity(nu);
        for (n, &x) in self.buf[off..off + nu].iter().enumerate() {
            if n % 64 == 0 {
                rot = Complex32::from_polar(1.0, (-ph0 + dph * n as f64) as f32);
            }
            v.push(x * rot);
            rot *= step;
        }
        self.ffts[self.mode.index()].process(&mut v);
        let scale = 1.0 / nu as f32;
        for (b, c) in v.iter_mut().enumerate() {
            let k = if b < nu / 2 { b as f64 } else { b as f64 - nu as f64 };
            *c *= Complex32::from_polar(scale, (TAU * k * d / nu as f64) as f32);
        }
        Some(v)
    }

    fn track(&mut self) -> bool {
        let pos = self.sym_pos;
        let Some(y) = self.fft_at(pos) else {
            return false;
        };
        let wi = (pos - self.mode.tg() as f64 / 2.0).floor().max(0.0) as u64;
        self.sym_pos += self.period;
        match self.phase {
            Phase::FreqInt => self.freq_int(y, wi),
            Phase::FrameSync => self.frame_sync(y, wi),
            Phase::Locked => self.locked(y, wi),
            Phase::Search => {}
        }
        self.count += 1;
        self.phase_count += 1;
        true
    }

    /// Products of each frequency reference's bin (shifted by `shift`
    /// bins) with the last symbol's, signed so that they are coherent from
    /// symbol to symbol, and their magnitudes.
    fn pilot_steps(&self, y: &[Complex32], shift: i32) -> [(Complex32, f32); 3] {
        let nu = self.mode.tu();
        let mut out = [(Complex32::new(0.0, 0.0), 0.0); 3];
        for (j, &(k, _)) in tables::freq_pilots(self.mode).iter().enumerate() {
            let b = carrier_bin(k + shift, nu);
            let mut v = y[b] * self.prev[b].conj();
            if self.mode == Mode::D && j < 2 {
                v = -v;
            }
            out[j] = (v, y[b].norm() * self.prev[b].norm());
        }
        out
    }

    fn symbol_seconds(&self) -> f64 {
        self.period / FS
    }

    fn freq_int(&mut self, y: Vec<Complex32>, wi: u64) {
        if !self.prev.is_empty() {
            let q = (self.int_acc.len() / 2) as i32;
            self.int_pow +=
                y.iter().zip(self.prev.iter()).map(|(a, b)| a.norm() * b.norm()).sum::<f32>() / y.len() as f32;
            for qi in 0..self.int_acc.len() {
                let steps = self.pilot_steps(&y, qi as i32 - q);
                for (acc, (a, m)) in self.int_acc[qi].iter_mut().zip(steps) {
                    acc.0 += a;
                    acc.1 += m;
                }
            }
        }
        self.prev = y;
        if self.phase_count < INT_PAIRS as u64 {
            return;
        }
        let q = (self.int_acc.len() / 2) as i32;
        // Each reference's coherent power against the spectrum's mean: the
        // references are boosted and keep their phase, data bins wander
        // and empty bins have nothing. A shift that lands a reference on
        // another is coherent there too, so such a reference does not
        // count for that shift; the rest are averaged, so that one or two
        // references in a fade (echoes cut notches every few carriers)
        // still leave the true shift standing out.
        let mean = self.int_pow.max(1e-30);
        let fp = tables::freq_pilots(self.mode);
        let score: Vec<f32> = self
            .int_acc
            .iter()
            .enumerate()
            .map(|(qi, p)| {
                let shift = qi as i32 - q;
                let (mut sum, mut n) = (0.0, 0);
                for (j, (a, _)) in p.iter().enumerate() {
                    let lands = fp.iter().enumerate().any(|(i, f)| i != j && f.0 == fp[j].0 + shift);
                    if !lands {
                        sum += a.norm() / mean;
                        n += 1;
                    }
                }
                if n > 0 { sum / n as f32 } else { 0.0 }
            })
            .collect();
        let (bi, &bs) = score.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap_or((0, &0.0));
        let second = score
            .iter()
            .enumerate()
            .filter(|(i, _)| (*i as i32 - bi as i32).abs() > 1)
            .map(|(_, &s)| s)
            .fold(0.0, f32::max);
        if bs > 1.5 && bs > 2.0 * second {
            let shift = bi as i32 - q;
            // The bins compared are `shift` carriers off, and a whole
            // carrier of offset turns a symbol (guard included) by
            // 2 pi Ts / Tu: take that out before reading the remainder.
            let sum: Complex32 = self.int_acc[bi].iter().map(|p| p.0).sum();
            let extra = TAU * shift as f64 * self.mode.tg() as f64 / self.mode.tu() as f64;
            let turn = (sum.arg() as f64 - extra + std::f64::consts::PI).rem_euclid(TAU) - std::f64::consts::PI;
            let resid = turn / (TAU * self.symbol_seconds());
            let hz = self.nco_hz + shift as f64 * self.mode.spacing_hz() + resid;
            self.set_nco(hz, wi);
            self.phase = Phase::FrameSync;
            self.phase_count = 0;
            self.prev.clear();
            let ns = self.mode.symbols_per_frame();
            self.tref = vec![([Complex32::new(0.0, 0.0); 5], 0.0); ns];
            self.power = vec![0.0; self.mode.tu()];
        } else if self.phase_count > 3 * INT_PAIRS as u64 {
            self.lose();
        }
    }

    /// Track the frequency from the frequency references' phase step.
    fn track_freq(&mut self, y: &[Complex32], wi: u64) {
        if !self.prev.is_empty() {
            let steps = self.pilot_steps(y, 0);
            let a: Complex32 = steps.iter().map(|p| p.0).sum();
            let m: f32 = steps.iter().map(|p| p.1).sum();
            if m > 0.0 {
                let err = a.arg() as f64 / (TAU * self.symbol_seconds());
                let hz = self.nco_hz + FREQ_GAIN * err;
                self.set_nco(hz, wi);
            }
        }
        self.prev = y.to_vec();
    }

    fn frame_sync(&mut self, y: Vec<Complex32>, wi: u64) {
        let ns = self.mode.symbols_per_frame();
        let nu = self.mode.tu();
        // Time references: products of nearby references, grouped by their
        // spacing so a timing error (a phase ramp) cannot cancel them.
        let tp = tables::time_pilots(self.mode);
        let mut groups = [Complex32::new(0.0, 0.0); 5];
        let mut norm = 0.0;
        for i in 0..tp.len() {
            for j in i + 1..tp.len() {
                let dk = tp[j].0 - tp[i].0;
                if dk > 4 {
                    break;
                }
                let pi = Complex32::from_polar(1.0, (TAU * tp[i].1 as f64 / 1024.0) as f32);
                let pj = Complex32::from_polar(1.0, (TAU * tp[j].1 as f64 / 1024.0) as f32);
                let a = y[carrier_bin(tp[i].0, nu)];
                let b = y[carrier_bin(tp[j].0, nu)];
                groups[dk as usize] += a * b.conj() * (pi.conj() * pj);
                norm += a.norm() * b.norm();
            }
        }
        // Summed per slot over the frames seen, as complex values: on the
        // time reference symbol they add up, elsewhere they average away.
        let slot = (self.count % ns as u64) as usize;
        for (acc, g) in self.tref[slot].0.iter_mut().zip(groups.iter()) {
            *acc += g;
        }
        self.tref[slot].1 += norm;
        for (p, v) in self.power.iter_mut().zip(y.iter()) {
            *p += v.norm_sqr();
        }
        self.track_freq(&y, wi);
        if self.phase_count + 1 < 2 * ns as u64 {
            return;
        }
        let metric: Vec<f32> = self
            .tref
            .iter()
            .map(|(g, n)| if *n > 0.0 { g.iter().map(|g| g.norm()).sum::<f32>() / n } else { 0.0 })
            .collect();
        let (bi, &bv) = metric.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap_or((0, &0.0));
        let others = (metric.iter().sum::<f32>() - bv) / (ns - 1) as f32;
        if bv > 0.3 && bv > 2.5 * others {
            self.frame_offset = bi as u64;
            let so = self.estimate_so();
            self.configure(so);
            self.phase = Phase::Locked;
            self.phase_count = 0;
        } else if self.phase_count > 5 * ns as u64 {
            self.lose();
        }
    }

    /// The spectrum occupancy whose band best splits the carriers' power
    /// into a high level inside and a low one outside (a two-level fit to
    /// the log power, the pilots' boost and fading notwithstanding).
    fn estimate_so(&self) -> u8 {
        let mode = self.mode;
        let nu = mode.tu();
        let occ = mode.occupancies();
        let lo = occ.iter().filter_map(|&s| tables::k_range(mode, s)).map(|r| r.0).min().unwrap_or(0) - 8;
        let hi = occ.iter().filter_map(|&s| tables::k_range(mode, s)).map(|r| r.1).max().unwrap_or(0) + 8;
        let lp: Vec<(i32, f32)> = (lo..=hi)
            .filter(|&k| !mode.unused(k))
            .map(|k| (k, (self.power[carrier_bin(k, nu)] + 1e-20).log10()))
            .collect();
        let mut best = (f32::INFINITY, occ[0]);
        for &so in occ {
            let Some((kmin, kmax)) = tables::k_range(mode, so) else { continue };
            let inside = |k: i32| k >= kmin && k <= kmax;
            let (mut si, mut ni, mut so_, mut no) = (0.0, 0, 0.0, 0);
            for &(k, l) in &lp {
                if inside(k) {
                    si += l;
                    ni += 1;
                } else {
                    so_ += l;
                    no += 1;
                }
            }
            let mi = si / ni.max(1) as f32;
            let mo = so_ / no.max(1) as f32;
            let cost: f32 = lp.iter().map(|&(k, l)| if inside(k) { (l - mi).powi(2) } else { (l - mo).powi(2) }).sum();
            if cost < best.0 {
                best = (cost, so);
            }
        }
        best.1
    }

    fn configure(&mut self, so: u8) {
        let Some(map) = CellMap::new(self.mode, so) else { return };
        self.est = Some(ChanEst::new(&map));
        self.frame = Some(FrameBuf { z: vec![Vec::new(); map.ns], w: vec![Vec::new(); map.ns], next: usize::MAX });
        self.map = Some(map);
        self.sf = SuperFrame::default();
        self.deint = None;
        self.sdc_mlc = None;
        self.msc_mlc = None;
    }

    fn locked(&mut self, y: Vec<Complex32>, wi: u64) {
        let ns = self.mode.symbols_per_frame() as u64;
        let s = ((self.count + ns - self.frame_offset % ns) % ns) as usize;
        let (Some(map), Some(est)) = (self.map.as_ref(), self.est.as_mut()) else { return };
        let nu = self.mode.tu();
        let car: Vec<Complex32> = (0..map.ncar).map(|i| y[carrier_bin(map.kmin + i as i32, nu)]).collect();
        let eq = est.push(self.count as i64, s, car, map);
        let delay = est.delay as f64;
        let turn = est.turn;
        // The timing loop steers on the mean delay of the channel.
        if self.phase_count > 2 * ns {
            let e = delay.clamp(-(self.mode.tg() as f64) / 4.0, self.mode.tg() as f64 / 4.0);
            self.sym_pos += TIME_KP * e;
            self.period += TIME_KI * e;
            if self.clock_fit[0] == 0.0 {
                self.clock_origin = (self.count, self.sym_pos);
            }
            let x = (self.count - self.clock_origin.0) as f64;
            let y = self.sym_pos - self.clock_origin.1 - x * self.mode.ts() as f64;
            let f = &mut self.clock_fit;
            *f = [f[0] + 1.0, f[1] + x, f[2] + y, f[3] + x * x, f[4] + x * y];
            if let Some(est) = self.est.as_mut() {
                est.shift((TIME_KP * e) as f32);
            }
        }
        // Frequency from the channel's turn between symbols (the
        // frequency references alone would also see the timing loop's
        // steps, and fall in the notches echoes make).
        if turn.norm() > 0.0 {
            let err = turn.arg() as f64 / (TAU * self.symbol_seconds());
            let hz = self.nco_hz + FREQ_GAIN * err;
            self.set_nco(hz, wi);
        }
        if let Some(eq) = eq {
            self.on_symbol(eq);
        }
    }

    // -----------------------------------------------------------------
    // Frames.

    fn on_symbol(&mut self, eq: Equalized) {
        let Some(frame) = self.frame.as_mut() else { return };
        let ns = frame.z.len();
        if eq.s == 0 {
            frame.next = 0;
        }
        if frame.next != eq.s {
            return;
        }
        frame.z[eq.s] = eq.z;
        frame.w[eq.s] = eq.w;
        frame.next += 1;
        if frame.next == ns {
            frame.next = usize::MAX;
            self.process_frame();
        }
    }

    fn process_frame(&mut self) {
        let Some(map) = self.map.as_ref() else { return };
        let Some(frame) = self.frame.as_ref() else { return };
        let (z, w): (Vec<Complex32>, Vec<f32>) = map.fac.iter().map(|&(s, i)| (frame.z[s][i], frame.w[s][i])).unzip();
        self.fac_mer.add(&z, &w, 1);
        let (mut bits, _) = self.fac_mlc.decode(&z, &w, &mut self.vit);
        coding::disperse(&mut bits);
        let fac = info::parse_fac(&bits);
        let so = map.so;
        let ns = map.ns;
        match fac {
            Some(f) => {
                self.stats.fac_ok += 1;
                self.fac_rate.add(true);
                self.fac_fail = 0;
                if f.so != so {
                    // The power profile guessed the occupancy wrong; the FAC
                    // knows.
                    self.configure(f.so);
                    self.fac = Some(f);
                    return;
                }
                self.frame_index = Some(f.frame());
                let sid = f.service.short_id as usize;
                self.services[sid].fac = Some(f.service.clone());
                if !self.fac_confirmed {
                    self.fac_confirmed = true;
                    self.events.push(DrmEvent::Sync { locked: true, mode: Some(self.mode.letter()), so: Some(f.so) });
                }
                self.fac = Some(f);
                self.report_service(sid);
            }
            None => {
                self.stats.fac_bad += 1;
                self.fac_rate.add(false);
                self.fac_fail += 1;
                self.frame_index = self.frame_index.map(|i| (i + 1) % 3);
                let limit = if self.fac_confirmed { FAC_LOST } else { FAC_NEVER };
                if self.fac_fail >= limit {
                    self.lose();
                    return;
                }
            }
        }
        let Some(fi) = self.frame_index else { return };
        if fi == 0 {
            self.decode_sdc();
        }
        // Gather the MSC cells of the super frame.
        if fi == 0 {
            self.sf = SuperFrame { z: Vec::new(), w: Vec::new(), next: Some(0) };
        }
        if self.sf.next != Some(fi) {
            self.sf.next = None;
            return;
        }
        let (Some(map), Some(frame)) = (self.map.as_ref(), self.frame.as_ref()) else { return };
        for s in 0..ns {
            for &i in &map.msc[fi as usize * ns + s] {
                self.sf.z.push(frame.z[s][i as usize]);
                self.sf.w.push(frame.w[s][i as usize]);
            }
        }
        if fi == 2 {
            self.sf.next = None;
            self.process_superframe();
        } else {
            self.sf.next = Some(fi + 1);
        }
    }

    fn decode_sdc(&mut self) {
        let (Some(map), Some(frame), Some(fac)) = (self.map.as_ref(), self.frame.as_ref(), self.fac.as_ref()) else {
            return;
        };
        let n = map.n_sdc();
        let q16 = fac.sdc_qam16;
        if self.sdc_mlc.as_ref().is_none_or(|(a, b, _)| *a != n || *b != q16) {
            let Some(m) = Mlc::sdc(n, q16) else { return };
            self.sdc_mlc = Some((n, q16, m));
        }
        let (z, w): (Vec<Complex32>, Vec<f32>) = map.sdc.iter().map(|&(s, i)| (frame.z[s][i], frame.w[s][i])).unzip();
        let Some((_, _, mlc)) = self.sdc_mlc.as_ref() else { return };
        let (mut bits, _) = mlc.decode(&z, &w, &mut self.vit);
        coding::disperse(&mut bits);
        self.sdc_seen = true;
        match info::parse_sdc(&bits) {
            Some(sdc) => {
                self.stats.sdc_ok += 1;
                self.sdc_rate.add(true);
                self.apply_sdc(sdc);
            }
            None => {
                self.stats.sdc_bad += 1;
                self.sdc_rate.add(false);
            }
        }
    }

    fn apply_sdc(&mut self, sdc: Sdc) {
        if let Some(m) = sdc.multiplex {
            self.multiplex = Some(m);
        }
        let mut touched = [false; 4];
        for (sid, label) in sdc.labels {
            let s = &mut self.services[sid as usize & 3];
            s.label = Some(label);
            touched[sid as usize & 3] = true;
        }
        for a in sdc.audio {
            let sid = a.short_id as usize & 3;
            self.services[sid].audio = Some(a);
            touched[sid] = true;
        }
        for l in sdc.languages {
            let sid = l.short_id as usize & 3;
            self.services[sid].lang = Some(l);
            touched[sid] = true;
        }
        if let Some(t) = sdc.time
            && self.time != Some(t)
        {
            self.time = Some(t);
            self.events.push(DrmEvent::Time {
                year: t.year,
                month: t.month,
                day: t.day,
                hour: t.hour,
                minute: t.minute,
                local_offset_min: t.local_offset_min,
            });
        }
        for (sid, t) in touched.iter().enumerate() {
            if *t {
                self.report_service(sid);
            }
        }
    }

    fn report_service(&mut self, sid: usize) {
        let st = &self.services[sid];
        let Some(fac) = &st.fac else { return };
        let audio = st.audio.as_ref();
        let svc = DrmService {
            short_id: sid as u8,
            service_id: fac.id,
            label: st.label.clone(),
            audio: !fac.data,
            language: Some(tables::language_name(fac.language)),
            language_code: st.lang.as_ref().map(|l| l.language.clone()),
            country_code: st.lang.as_ref().map(|l| l.country.clone()),
            program_type: (!fac.data).then(|| tables::program_type_name(fac.descriptor)),
            codec: audio.map(|a| AudioCodec::from_code(a.coding)),
            sample_rate_hz: audio.and_then(|a| a.sample_rate_hz()),
            sbr: audio.is_some_and(|a| a.sbr && a.coding == 0),
            audio_mode: audio.map(|a| match a.audio_mode {
                0 => "mono",
                1 => "parametric stereo",
                2 => "stereo",
                _ => "reserved",
            }),
            text: audio.is_some_and(|a| a.text),
            ca: fac.audio_ca || fac.data_ca,
        };
        if st.reported.as_ref() != Some(&svc) {
            self.services[sid].reported = Some(svc.clone());
            self.events.push(DrmEvent::Service(svc));
        }
    }

    // -----------------------------------------------------------------
    // The MSC.

    fn process_superframe(&mut self) {
        let Some(map) = self.map.as_ref() else { return };
        let Some(fac) = self.fac.clone() else { return };
        let n_mux = map.n_mux;
        let sf = std::mem::take(&mut self.sf);
        if sf.z.len() < 3 * n_mux {
            return;
        }
        let levels = match fac.msc_mode {
            0 => 3,
            3 => 2,
            _ => return,
        };
        if self.deint.as_ref().is_none_or(|d| d.n_mux() != n_mux || d.long() != fac.long_interleave) {
            self.deint = Some(CellDeinterleaver::new(n_mux, fac.long_interleave));
        }
        for j in 0..3 {
            let r = j * n_mux..(j + 1) * n_mux;
            let Some(d) = self.deint.as_mut() else { return };
            if let Some((z, w)) = d.push(&sf.z[r.clone()], &sf.w[r]) {
                self.decode_mux(&z, &w, levels);
            }
        }
        self.stats.freq_offset_hz = self.nco_hz as f32;
        self.stats.clock_ppm = self.clock_ppm();
        if let Some((m, wm)) = self.msc_mer.db().or(self.fac_mer.db()) {
            self.stats.mer_db = Some(m);
            self.stats.wmer_db = Some(wm);
        }
        self.events.push(DrmEvent::Stats(self.stats.clone()));
    }

    fn decode_mux(&mut self, z: &[Complex32], w: &[f32], levels: usize) {
        self.msc_mer.add(z, w, levels);
        let take = z.len().min(CONSTELLATION_CELLS);
        self.constellation = z[z.len() - take..].to_vec();
        let Some(mux) = self.multiplex.clone() else { return };
        let key = (z.len(), levels, mux.pl_a, mux.pl_b, mux.len_a());
        if self.msc_mlc.as_ref().is_none_or(|(k, _)| *k != key) {
            let Some(m) = Mlc::msc(z.len(), levels, mux.pl_a, mux.pl_b, mux.len_a()) else { return };
            self.msc_mlc = Some((key, m));
        }
        let Some((_, mlc)) = self.msc_mlc.as_ref() else { return };
        let (mut bits, ber) = mlc.decode(z, w, &mut self.vit);
        self.stats.msc_ber = Some(ber);
        coding::disperse(&mut bits);
        let bytes = coding::pack(&bits[..bits.len() / 8 * 8]);
        for sid in 0..4 {
            let (Some(a), Some(f)) = (self.services[sid].audio.clone(), self.services[sid].fac.clone()) else {
                continue;
            };
            // A receiver without conditional access must not decode a
            // scrambled audio stream (clause 6.3.4).
            if f.data || f.audio_ca {
                continue;
            }
            let Some((stream, len_a)) = mux.stream(&bytes, a.stream_id as usize) else { continue };
            let mut asf = stream.as_slice();
            if a.text && asf.len() >= 4 {
                let (head, piece) = asf.split_at(asf.len() - 4);
                asf = head;
                match self.text[sid].push(piece) {
                    Some(TextUpdate::Message(t)) => {
                        self.events.push(DrmEvent::Text { short_id: sid as u8, service_id: f.id, text: t })
                    }
                    Some(TextUpdate::Clear) => {
                        self.events.push(DrmEvent::Text { short_id: sid as u8, service_id: f.id, text: String::new() })
                    }
                    None => {}
                }
            }
            let codec = AudioCodec::from_code(a.coding);
            let Some(rate) = a.sample_rate_hz() else { continue };
            let frames = match codec {
                AudioCodec::Aac => {
                    let n = match rate {
                        12_000 => 5,
                        24_000 => 10,
                        _ => continue,
                    };
                    audio::split_aac(asf, len_a, n)
                }
                AudioCodec::XheAac => self.xhe[sid].push(asf),
                _ => continue,
            };
            let ok = !frames.is_empty() && frames.iter().all(|f| f.ok);
            if ok {
                self.stats.msc_ok += 1;
                self.msc_rate.add(true);
            } else {
                self.stats.msc_bad += 1;
                self.msc_rate.add(false);
            }
            self.msc_seen = true;
            self.events.push(DrmEvent::AudioSuperFrame {
                service: sid as u8,
                codec,
                sample_rate_hz: rate,
                sbr: a.sbr && a.coding == 0,
                stereo: a.audio_mode == 2,
                ps: a.audio_mode == 1 && a.coding == 0,
                audio_config: a.raw.clone(),
                frames,
            });
        }
    }
}
