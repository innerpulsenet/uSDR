//! One conventional channel: IQ in, calls and audio out.
//!
//! This is where the pieces meet. Wideband IQ from the device is translated and
//! decimated to the channel ([`DecodeChain`]), demodulated ([`NbfmDemod`]),
//! watched for a sub-audible tone ([`CtcssDetector`]) and gated by a
//! [`Squelch`] driven from the span-wide SNR.
//!
//! Audio is buffered continuously rather than only while the squelch is open, so
//! a call can be emitted with a short **pre-roll**. Squelch decisions lag the
//! signal by an FFT and a hang constant, and without pre-roll every recording
//! loses the first syllable — the one carrying the unit identifier.

use crate::afc::Afc;
use crate::ctcss::CtcssDetector;
use crate::dcs::DcsDetector;
use crate::mdc::{MdcDecoder, MdcPacket};
use crate::nbfm::{Demodulated, NBFM_BANDWIDTH_HZ, NbfmDemod};
use crate::noisegate::NoiseGate;
use crate::squelch::Squelch;
use num_complex::Complex32;
use scannerd_dsp::DecodeChain;
use std::collections::VecDeque;

/// Audio kept before a call starts, in seconds.
pub const PREROLL_S: f32 = 1.25;

/// Audio sample rate handed to the encoder. Opus takes 16 kHz natively, and it
/// leaves room above the ~3 kHz of land-mobile voice for the filters to work in.
pub const AUDIO_RATE: f64 = 16_000.0;

/// The sub-audible squelch code a channel is supposed to carry.
///
/// Land mobile uses two incompatible schemes and they need telling apart.
/// **CTCSS** ("PL") is a continuous tone, decoded here by a Goertzel bank.
/// **DCS** ("DPL") is a 23-bit Golay codeword clocked at 134.4 bps.
#[derive(Clone, Debug, PartialEq)]
pub enum SquelchCode {
    None,
    /// CTCSS tone in Hz, e.g. 82.5.
    Ctcss(f32),
    /// DCS code as the conventional three-digit octal number, e.g. 0o071.
    Dcs(u16),
}

impl SquelchCode {
    pub fn label(&self) -> String {
        match self {
            SquelchCode::None => "-".into(),
            SquelchCode::Ctcss(hz) => format!("{hz:.1} PL"),
            SquelchCode::Dcs(code) => format!("{code:03o} DPL"),
        }
    }

    /// Whether this scheme can be confirmed against the air.
    pub fn is_verifiable(&self) -> bool {
        !matches!(self, SquelchCode::None)
    }
}

/// A sub-audible code actually decoded during a conventional call.
#[derive(Clone, Debug, PartialEq)]
pub enum ToneCode {
    Ctcss(f32),
    Dcs(u16),
    /// DMR colour code, and the timeslot if it was identified.
    Dmr {
        color_code: u8,
        slot: Option<u8>,
    },
    /// P25 Network Access Code.
    P25 {
        nac: u16,
    },
}

impl ToneCode {
    pub fn label(&self) -> String {
        match self {
            Self::Ctcss(hz) => format!("{hz:.1} PL"),
            Self::Dcs(code) => format!("{code:03o} DPL"),
            Self::Dmr {
                color_code,
                slot: Some(slot),
            } => format!("CC{color_code} TS{slot}"),
            Self::Dmr {
                color_code,
                slot: None,
            } => format!("CC{color_code}"),
            Self::P25 { nac } => format!("NAC ${nac:03X}"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ChannelSpec {
    pub name: String,
    pub freq_hz: f64,
    pub bandwidth_hz: f64,
    /// What the channel is documented to transmit.
    pub expected: SquelchCode,
}

impl ChannelSpec {
    pub fn new(name: impl Into<String>, freq_hz: f64) -> Self {
        Self {
            name: name.into(),
            freq_hz,
            bandwidth_hz: NBFM_BANDWIDTH_HZ,
            expected: SquelchCode::None,
        }
    }

    pub fn with_code(mut self, code: SquelchCode) -> Self {
        self.expected = code;
        self
    }
}

/// What a call turned out to be, once it ended.
#[derive(Clone, Debug, PartialEq)]
pub struct CallSummary {
    pub audio_samples: usize,
    pub peak_snr_db: f32,
    pub tone: Option<ToneCode>,
    /// MDC-1200 PTT-ID burst decoded during the call, if any.
    pub mdc: Option<MdcPacket>,
    /// How far off frequency the transmission was, in Hz. Large values mean the
    /// channel is misconfigured, not that the transmitter is drifting.
    pub freq_error_hz: f32,
    /// Fraction of the call the noise gate let through, in `0.0..=1.0`.
    ///
    /// A low value means most of what the squelch captured was carrier-less
    /// noise — a call that is nearly all pre-roll and hang, and probably not
    /// worth listening to.
    pub voiced_fraction: f32,
    /// Protocol-neutral identity and security metadata for conventional
    /// digital calls. P25 trunk calls carry their richer telemetry separately.
    pub digital: Option<DigitalCallTelemetry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DigitalCallTelemetry {
    pub protocol: String,
    /// DMR colour code (0-15), when the protocol carries one.
    pub color_code: Option<u8>,
    /// One-based DMR timeslot, when recovered from CACH.
    pub slot: Option<u8>,
    pub source_id: Option<u32>,
    pub target_id: Option<u32>,
    pub group: Option<bool>,
    pub manufacturer: Option<String>,
    pub service_options: Option<u8>,
    pub emergency: bool,
    pub encrypted: bool,
    /// True when encrypted voice was successfully deciphered with a configured
    /// key. Encrypted calls without this flag are metadata-only.
    pub decrypted: bool,
    pub algorithm_id: Option<u8>,
    pub key_id: Option<u16>,
    pub talker_alias: Option<String>,
    /// Smoothed voice-frame error metric, 0-100 — how heavily the demodulator
    /// was correcting the frames that carried the voice. A call that *plays*
    /// scrambled usually reads high here, which distinguishes a weak-but-real
    /// signal from a clean one. DMR only; `None` where the protocol exposes
    /// nothing comparable.
    pub bit_error_pct: Option<u8>,
}

impl CallSummary {
    pub fn duration_s(&self) -> f32 {
        self.audio_samples as f32 / AUDIO_RATE as f32
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum CallEvent {
    /// A call began. Any audio drained on this pass includes the pre-roll.
    Started,
    /// A call ended, with what was learned about it.
    Ended(CallSummary),
}

pub struct ChannelReceiver {
    pub spec: ChannelSpec,
    chain: DecodeChain,
    afc: Afc,
    demod: NbfmDemod,
    ctcss: CtcssDetector,
    dcs: DcsDetector,
    mdc_dec: MdcDecoder,
    gate: NoiseGate,
    leveler: crate::leveler::Leveler,
    squelch: Squelch,
    fs_out: f64,

    baseband: Vec<Complex32>,
    demodulated: Demodulated,
    preroll: VecDeque<f32>,
    preroll_cap: usize,

    /// Audio for the current call, drained by the caller each pass.
    out: Vec<f32>,
    in_call: bool,
    call_samples: usize,
    peak_snr_db: f32,
    tone: Option<ToneCode>,
    mdc: Option<MdcPacket>,
    /// Set when `mdc` was filled since the last [`take_mdc`](Self::take_mdc),
    /// so the scanner can publish a live in-call event exactly once per packet.
    mdc_new: bool,
    code_matched: bool,
    last_carrier: bool,
    offset_sum: f64,
    offset_n: usize,
    duty_sum: f64,
    duty_n: usize,
}

impl ChannelReceiver {
    pub fn new(spec: ChannelSpec, fs_in: f64, span_center_hz: f64) -> Self {
        let chain = DecodeChain::new(fs_in, spec.bandwidth_hz as f32, AUDIO_RATE);
        let fs_out = chain.fs_out();
        let mut rx = Self {
            chain,
            // One NBFM deviation: enough to centre a mistuned channel inside
            // the 11 kHz IF, not enough to walk onto a neighbour.
            afc: Afc::new(0.0, 2_500.0),
            demod: NbfmDemod::new(fs_out),
            ctcss: CtcssDetector::new(fs_out),
            dcs: DcsDetector::new(fs_out),
            mdc_dec: MdcDecoder::new(fs_out),
            gate: NoiseGate::new(fs_out),
            leveler: crate::leveler::Leveler::new(fs_out),
            squelch: Squelch::default(),
            fs_out,
            baseband: Vec::new(),
            demodulated: Demodulated::default(),
            preroll: VecDeque::new(),
            preroll_cap: (PREROLL_S * fs_out as f32) as usize,
            out: Vec::new(),
            in_call: false,
            call_samples: 0,
            peak_snr_db: f32::NEG_INFINITY,
            tone: None,
            mdc: None,
            mdc_new: false,
            code_matched: false,
            last_carrier: false,
            offset_sum: 0.0,
            offset_n: 0,
            duty_sum: 0.0,
            duty_n: 0,
            spec,
        };
        rx.retune(span_center_hz);
        rx
    }

    /// Point the channel extractor at this channel within the current span.
    pub fn retune(&mut self, span_center_hz: f64) {
        self.afc.set_base(self.spec.freq_hz - span_center_hz);
        self.chain.set_offset(self.afc.mix_hz());
    }

    /// Drop call, tone, and gate state. Used when the radio leaves the
    /// conventional span (P25 follow, hop) so a leftover `in_call` cannot
    /// keep recording after the tuner has moved.
    pub fn reset(&mut self) {
        self.squelch.reset();
        self.gate.reset();
        self.ctcss.reset();
        self.dcs.reset();
        self.mdc_dec.reset();
        self.demod.reset();
        self.leveler.reset();
        self.preroll.clear();
        self.out.clear();
        self.in_call = false;
        self.last_carrier = false;
        self.code_matched = false;
        self.tone = None;
        self.mdc = None;
        self.mdc_new = false;
        self.call_samples = 0;
        self.peak_snr_db = f32::NEG_INFINITY;
        self.offset_sum = 0.0;
        self.offset_n = 0;
        self.duty_sum = 0.0;
        self.duty_n = 0;
    }

    pub fn squelch_mut(&mut self) -> &mut Squelch {
        &mut self.squelch
    }

    pub fn gate_mut(&mut self) -> &mut NoiseGate {
        &mut self.gate
    }

    /// Whether audio is passing right now, as distinct from whether a call is
    /// open. False through pre-roll, hang, and dropouts.
    pub fn passing_audio(&self) -> bool {
        self.gate.is_open()
    }

    pub fn audio_rate(&self) -> f64 {
        self.fs_out
    }

    /// Audio produced on the most recent [`process`](Self::process).
    pub fn audio(&self) -> &[f32] {
        &self.out
    }

    /// Demodulate a block and advance the call state machine.
    ///
    /// `snr_db` comes from the span detector, which updates less often than
    /// samples arrive; passing the most recent reading each time is correct.
    pub fn process(&mut self, iq: &[Complex32], snr_db: f32) -> Option<CallEvent> {
        self.out.clear();
        self.chain.process(iq, &mut self.baseband);
        if self.baseband.is_empty() {
            return None;
        }
        let dt_s = self.baseband.len() as f32 / self.fs_out as f32;

        let baseband = std::mem::take(&mut self.baseband);
        self.demod.process(&baseband, &mut self.demodulated);
        self.baseband = baseband;

        // MDC-1200 rides the voice band. The decoder sees raw discriminator
        // audio (with internal DC blocking) so Mark (1200 Hz) and Space (1800 Hz)
        // have equal amplitudes without de-emphasis roll-off, before the noise gate
        // and leveler mutate the audio.
        if let Some(pkt) = self.mdc_dec.push(&self.demodulated.raw) {
            self.mdc = Some(pkt);
            self.mdc_new = true;
        }

        // Gate before anything downstream sees the audio, so the pre-roll
        // buffer, the recording and the live stream all get the same treatment.
        // The leveler follows it: gate-closed audio is far below the leveler's
        // speech floor, so its gain freezes instead of boosting static, and
        // the leveler's high-pass keeps any sub-audible tone out of what the
        // listener hears (tone detection reads `subaudible`, untouched here).
        let noise = std::mem::take(&mut self.demodulated.noise);
        self.gate.process(&mut self.demodulated.voice, &noise);
        self.demodulated.noise = noise;
        self.leveler.process(&mut self.demodulated.voice);
        self.duty_sum += f64::from(self.gate.duty());
        self.duty_n += 1;

        // Call lifetime is RF energy. Quieting still gates the audio, but
        // using it to *open* a call pinned the multi-span scanner: any
        // no-tone channel whose noise gate flickered held the tuner forever.
        let rf_open = self.squelch.update(snr_db, dt_s);
        let have_rf = baseband_power(&self.baseband) > MIN_CARRIER_POWER;
        let carrier_open = rf_open;

        // Steer the NCO from the residual discriminator DC. Frozen when
        // there is no carrier — including the energy-squelch hang, whose
        // input is noise that would drag the mix back toward zero.
        // The recorded offset is correction + residual: after AFC locks
        // the residual is ~0, and the correction is the true LO error.
        let tracking = have_rf && (self.gate.is_open() || snr_db >= self.squelch.close_db);
        if tracking && !self.demodulated.voice.is_empty() {
            self.afc.observe(self.demodulated.mean_offset_hz, true);
            self.chain.set_offset(self.afc.mix_hz());
            let total = self.afc.correction_hz() + self.demodulated.mean_offset_hz;
            self.offset_sum += f64::from(total) * self.demodulated.voice.len() as f64;
            self.offset_n += self.demodulated.voice.len();
        }

        let was_carrier = self.last_carrier;
        self.last_carrier = carrier_open;
        if !was_carrier && carrier_open {
            self.ctcss.reset();
            self.dcs.reset();
            self.tone = None;
            self.code_matched = matches!(self.spec.expected, SquelchCode::None);
            let total = self.afc.correction_hz() + self.demodulated.mean_offset_hz;
            self.offset_sum = f64::from(total) * self.demodulated.voice.len() as f64;
            self.offset_n = self.demodulated.voice.len();
        }
        if was_carrier && !carrier_open && !self.in_call {
            self.mdc = None;
            self.mdc_new = false;
        }

        if carrier_open {
            let ctcss = match self.spec.expected {
                SquelchCode::Ctcss(expected) => self
                    .ctcss
                    .push_expected(&self.demodulated.subaudible, expected),
                SquelchCode::None => self.ctcss.push(&self.demodulated.subaudible),
                SquelchCode::Dcs(_) => None,
            };
            if let Some(Some(detection)) = ctcss {
                let decoded = ToneCode::Ctcss(detection.tone_hz);
                if matches!(self.spec.expected, SquelchCode::None)
                    || matches!(self.spec.expected, SquelchCode::Ctcss(expected) if (expected - detection.tone_hz).abs() < 0.2)
                {
                    self.code_matched = true;
                    self.tone = Some(decoded);
                }
            }
            let dcs = if matches!(self.spec.expected, SquelchCode::None | SquelchCode::Dcs(_)) {
                self.dcs.push(&self.demodulated.subaudible)
            } else {
                None
            };
            if let Some(Some(detection)) = dcs {
                let decoded = ToneCode::Dcs(detection.code);
                if matches!(self.spec.expected, SquelchCode::None)
                    || matches!(self.spec.expected, SquelchCode::Dcs(expected) if expected == detection.code)
                {
                    self.code_matched = true;
                    // A valid repeated Golay word is more specific than a
                    // spectral CTCSS guess on the same low-frequency data.
                    self.tone = Some(decoded);
                }
            }
        }

        let was_open = self.in_call;
        // RF energy opens the hang, but a call only *starts* once the
        // discriminator quiets. A hop or P25-follow retune spikes the
        // span detector for a couple of blocks with no carrier; that used
        // to dump 1.25 s of pre-roll plus 1.5 s of hang into a 0 % voiced
        // recording and look like analog being cut off at ~2.8 s.
        let open = carrier_open && self.code_matched && (was_open || self.gate.is_open());

        // Keep the rolling pre-roll fed whether or not a call is running; it
        // costs a few hundred milliseconds of memory and is the difference
        // between catching the first syllable and losing it.
        if !open {
            for &v in &self.demodulated.voice {
                if self.preroll.len() == self.preroll_cap {
                    self.preroll.pop_front();
                }
                self.preroll.push_back(v);
            }
        }

        match (was_open, open) {
            (false, true) => {
                self.in_call = true;
                self.call_samples = 0;
                self.peak_snr_db = snr_db;
                self.duty_sum = f64::from(self.gate.duty());
                self.duty_n = 1;
                self.out.extend(self.preroll.drain(..));
                self.out.extend_from_slice(&self.demodulated.voice);
                self.call_samples += self.out.len();
                Some(CallEvent::Started)
            }
            (true, true) => {
                self.peak_snr_db = self.peak_snr_db.max(snr_db);
                self.out.extend_from_slice(&self.demodulated.voice);
                self.call_samples += self.demodulated.voice.len();
                None
            }
            (true, false) => Some(CallEvent::Ended(self.end_call())),
            (false, false) => None,
        }
    }

    /// End an in-progress call and produce its summary.
    fn end_call(&mut self) -> CallSummary {
        self.in_call = false;
        let summary = CallSummary {
            audio_samples: self.call_samples,
            peak_snr_db: self.peak_snr_db,
            tone: self.tone.clone(),
            mdc: self.mdc.clone(),
            freq_error_hz: if self.offset_n > 0 {
                (self.offset_sum / self.offset_n as f64) as f32
            } else {
                0.0
            },
            voiced_fraction: if self.duty_n > 0 {
                (self.duty_sum / self.duty_n as f64) as f32
            } else {
                0.0
            },
            digital: None,
        };
        self.peak_snr_db = f32::NEG_INFINITY;
        self.mdc = None;
        self.mdc_new = false;
        summary
    }

    /// End an in-progress call because the receiver is being taken away --
    /// a span hop, or a trunking grant borrowing the radio. Conventional
    /// operation normally ends from the squelch hang instead.
    ///
    /// Without this a hop left the call open: the recording stayed in the
    /// scanner's `live` slot with no channel feeding it, and only closed the
    /// next time the span came back around, which filed the tail of the call
    /// into the log *after* whatever was recorded in between.
    pub fn finish(&mut self) -> Option<CallEvent> {
        if !self.in_call {
            return None;
        }
        Some(CallEvent::Ended(self.end_call()))
    }

    pub fn in_call(&self) -> bool {
        self.in_call
    }

    /// The most recently decoded MDC packet, handed out exactly once.
    ///
    /// The call summary still carries `mdc` at end of call; this exists so a
    /// live event can be published mid-call without repeating the same packet
    /// on every block. A new carrier resets the flag with the rest of the
    /// per-call state.
    pub fn take_mdc(&mut self) -> Option<MdcPacket> {
        if !self.mdc_new {
            return None;
        }
        self.mdc_new = false;
        self.mdc.clone()
    }
}

/// Mean |z|² of a baseband block. Used to tell a quieted carrier from a
/// stream of zeros, which the noise gate cannot.
fn baseband_power(iq: &[Complex32]) -> f32 {
    if iq.is_empty() {
        return 0.0;
    }
    iq.iter().map(|c| c.norm_sqr()).sum::<f32>() / iq.len() as f32
}

/// Well below the 8-bit RTL-SDR noise floor; only all-zero IQ fails this.
const MIN_CARRIER_POWER: f32 = 1e-8;

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    const FS_IN: f64 = 1_024_000.0;
    const CENTER: f64 = 155_000_000.0;
    const CHAN: f64 = 155_007_500.0;
    /// One 16384-sample block decimates to 256 audio samples: 16 ms.
    const BLOCK: usize = 16384;

    /// A continuous FM source.
    ///
    /// The sample index has to persist across blocks. An earlier version
    /// restarted it at zero each call, which put a phase discontinuity in both
    /// the carrier and the modulation every block — enough to stop the tone
    /// decoder resolving a CTCSS tone that was, in fact, being transmitted.
    struct Source {
        k: u64,
        phase: f64,
    }

    impl Source {
        fn new() -> Self {
            Self { k: 0, phase: 0.0 }
        }

        fn fm(&mut self, n: usize, tone_hz: f64, dev_hz: f64, amp: f32) -> Vec<Complex32> {
            let offset = CHAN - CENTER;
            (0..n)
                .map(|_| {
                    let t = self.k as f64 / FS_IN;
                    self.phase += TAU * dev_hz * (TAU * tone_hz * t).sin() / FS_IN;
                    let a = TAU * offset * t + self.phase;
                    self.k += 1;
                    Complex32::new(amp * a.cos() as f32, amp * a.sin() as f32)
                })
                .collect()
        }

        fn dcs(&mut self, n: usize, code: u16, dev_hz: f64, amp: f32) -> Vec<Complex32> {
            let offset = CHAN - CENTER;
            let word = crate::dcs::transmitted_word(code);
            (0..n)
                .map(|_| {
                    let t = self.k as f64 / FS_IN;
                    let bit = ((t * 134.4) as usize) % 23;
                    let one = ((word >> (22 - bit)) & 1) != 0;
                    self.phase += TAU * if one { dev_hz } else { -dev_hz } / FS_IN;
                    let a = TAU * offset * t + self.phase;
                    self.k += 1;
                    Complex32::new(amp * a.cos() as f32, amp * a.sin() as f32)
                })
                .collect()
        }

        fn silence(&mut self, n: usize) -> Vec<Complex32> {
            self.k += n as u64;
            vec![Complex32::new(0.0, 0.0); n]
        }

        /// FM-modulate a 16 kHz audio waveform (e.g. an MDC burst), held
        /// across the decimation ratio. `pos` persists across blocks.
        fn waveform(
            &mut self,
            n: usize,
            audio: &[f32],
            pos: &mut usize,
            dev_hz: f64,
            amp: f32,
        ) -> Vec<Complex32> {
            let offset = CHAN - CENTER;
            let ratio = (FS_IN / AUDIO_RATE) as usize;
            (0..n)
                .map(|_| {
                    let t = self.k as f64 / FS_IN;
                    let m = f64::from(audio.get(*pos / ratio).copied().unwrap_or(0.0));
                    *pos += 1;
                    self.phase += TAU * dev_hz * m / FS_IN;
                    let a = TAU * offset * t + self.phase;
                    self.k += 1;
                    Complex32::new(amp * a.cos() as f32, amp * a.sin() as f32)
                })
                .collect()
        }

        /// Uncorrelated IQ, what the RTL-SDR delivers on an empty channel.
        fn noise(&mut self, n: usize) -> Vec<Complex32> {
            let mut s = self.k as u32 ^ 0xA5A5_1234;
            self.k += n as u64;
            (0..n)
                .map(|_| {
                    s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                    let i = (s >> 8) as f32 / 8388608.0 - 1.0;
                    s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                    let q = (s >> 8) as f32 / 8388608.0 - 1.0;
                    Complex32::new(i * 0.05, q * 0.05)
                })
                .collect()
        }
    }

    fn receiver() -> ChannelReceiver {
        ChannelReceiver::new(ChannelSpec::new("test", CHAN), FS_IN, CENTER)
    }

    /// Drive silence until the call closes, returning its summary.
    fn drain_to_end(rx: &mut ChannelReceiver, src: &mut Source) -> CallSummary {
        // The 1.5 s hang needs ~94 blocks of 16 ms; allow generous headroom.
        for _ in 0..300 {
            let quiet = src.silence(BLOCK);
            if let Some(CallEvent::Ended(s)) = rx.process(&quiet, -5.0) {
                return s;
            }
        }
        panic!("the call never ended");
    }

    #[test]
    fn the_chain_lands_on_the_configured_audio_rate() {
        assert_eq!(receiver().audio_rate(), AUDIO_RATE);
    }

    #[test]
    fn a_signal_starts_a_call_and_its_removal_ends_one() {
        let mut rx = receiver();
        let mut src = Source::new();
        let mut started = false;
        for _ in 0..8 {
            let b = src.fm(BLOCK, 1000.0, 2500.0, 0.5);
            if rx.process(&b, 20.0) == Some(CallEvent::Started) {
                started = true;
            }
        }
        assert!(started, "a strong signal should have started a call");
        assert!(rx.in_call());

        let summary = drain_to_end(&mut rx, &mut src);
        assert!(summary.peak_snr_db >= 20.0);
        assert!(summary.duration_s() > 0.0);
        assert!(!rx.in_call());
    }

    /// A span hop takes the radio away mid-call. `finish` has to end that
    /// call properly rather than leaving it open.
    ///
    /// The scanner only services the span it is parked on, so a call left
    /// open by a hop had nothing feeding it until the span came back around
    /// -- which filed the tail of the call into the log after whatever was
    /// recorded in between, and left the channel reading "active" throughout.
    #[test]
    fn finish_ends_a_call_that_a_hop_interrupts() {
        let mut rx = receiver();
        let mut src = Source::new();
        for _ in 0..8 {
            let b = src.fm(BLOCK, 1000.0, 2500.0, 0.5);
            rx.process(&b, 20.0);
        }
        assert!(rx.in_call(), "expected a call to be running");

        let ended = rx.finish();
        let Some(CallEvent::Ended(summary)) = ended else {
            panic!("finish() must end a running call, got {ended:?}");
        };
        assert!(summary.duration_s() > 0.0, "summary should carry the audio");
        assert!(summary.peak_snr_db >= 20.0, "summary should carry the SNR");
        assert!(!rx.in_call(), "the call must not still be open");

        // Idempotent: nothing left to end.
        assert!(rx.finish().is_none(), "finish() twice must not file twice");
    }

    /// An idle receiver has no call to end.
    #[test]
    fn finish_on_an_idle_channel_reports_nothing() {
        let mut rx = receiver();
        let mut src = Source::new();
        for _ in 0..4 {
            let b = src.noise(BLOCK);
            rx.process(&b, 0.0);
        }
        assert!(!rx.in_call());
        assert!(rx.finish().is_none());
    }

    /// The reason the pre-roll exists: audio from before the squelch opened has
    /// to be in the recording.
    #[test]
    fn a_call_opens_with_preroll_already_buffered() {
        let mut rx = receiver();
        let mut src = Source::new();
        // Signal present but below the squelch threshold, so audio accumulates
        // in the pre-roll without a call starting.
        for _ in 0..6 {
            let b = src.fm(BLOCK, 1000.0, 2500.0, 0.5);
            assert!(rx.process(&b, 0.0).is_none());
        }
        let b = src.fm(BLOCK, 1000.0, 2500.0, 0.5);
        assert_eq!(rx.process(&b, 20.0), Some(CallEvent::Started));
        let one_block = BLOCK / (FS_IN / AUDIO_RATE) as usize;
        assert!(
            rx.audio().len() > one_block,
            "first pass emitted {} samples, no more than the block itself ({one_block})",
            rx.audio().len()
        );
    }

    /// A CTCSS tone present through the call must end up on its summary.
    #[test]
    fn the_ctcss_tone_is_reported_on_the_summary() {
        let mut rx = receiver();
        let mut src = Source::new();
        // The tone decoder needs a full 1.024 s window: 64 blocks of 16 ms.
        for _ in 0..100 {
            let b = src.fm(BLOCK, 192.8, 500.0, 0.5);
            rx.process(&b, 20.0);
        }
        assert_eq!(
            drain_to_end(&mut rx, &mut src).tone,
            Some(ToneCode::Ctcss(192.8))
        );
    }

    #[test]
    fn configured_ctcss_must_match_before_the_call_opens() {
        let spec = ChannelSpec::new("test", CHAN).with_code(SquelchCode::Ctcss(192.8));
        let mut rx = ChannelReceiver::new(spec, FS_IN, CENTER);
        let mut src = Source::new();
        for _ in 0..80 {
            let b = src.fm(BLOCK, 100.0, 500.0, 0.5);
            assert_ne!(rx.process(&b, 20.0), Some(CallEvent::Started));
        }
        assert!(!rx.in_call(), "the wrong PL tone opened the channel");

        // Start a new carrier so both tone detectors and their vote reset.
        for _ in 0..120 {
            let quiet = src.silence(BLOCK);
            rx.process(&quiet, -5.0);
        }
        let mut started = false;
        for _ in 0..80 {
            let b = src.fm(BLOCK, 192.8, 500.0, 0.5);
            started |= rx.process(&b, 20.0) == Some(CallEvent::Started);
        }
        assert!(started, "the assigned PL tone did not open the channel");
    }

    #[test]
    fn configured_dcs_opens_only_after_repeated_words_decode() {
        let spec = ChannelSpec::new("test", CHAN).with_code(SquelchCode::Dcs(0o532));
        let mut rx = ChannelReceiver::new(spec, FS_IN, CENTER);
        let mut src = Source::new();
        let mut started = false;
        for _ in 0..80 {
            let b = src.dcs(BLOCK, 0o532, 500.0, 0.5);
            started |= rx.process(&b, 20.0) == Some(CallEvent::Started);
        }
        assert!(started, "the assigned DPL code did not open the channel");
        assert!(rx.in_call());
        assert_eq!(
            drain_to_end(&mut rx, &mut src).tone,
            Some(ToneCode::Dcs(0o532))
        );
    }

    /// An MDC-1200 PTT-ID burst inside the call must land on its summary.
    #[test]
    fn an_mdc_ptt_id_is_reported_on_the_summary() {
        let mut rx = receiver();
        let mut src = Source::new();
        // Plain voice first, so the squelch and gate open the call.
        for _ in 0..30 {
            let b = src.fm(BLOCK, 1000.0, 2500.0, 0.5);
            rx.process(&b, 20.0);
        }
        assert!(rx.in_call(), "voice should have opened the call");
        // Then the burst at typical MDC deviation (±750 Hz). 184 bits at
        // 1200 baud is ~2 450 audio samples, about ten blocks.
        let burst = crate::mdc::burst(&[[0x01, 0x00, 0x1A, 0x2B]], false);
        let mut pos = 0;
        for _ in 0..16 {
            let b = src.waveform(BLOCK, &burst, &mut pos, 750.0, 0.5);
            rx.process(&b, 20.0);
        }
        let summary = drain_to_end(&mut rx, &mut src);
        assert_eq!(
            summary.mdc,
            Some(crate::mdc::MdcPacket {
                op: 0x01,
                arg: 0x00,
                unit_id: 0x1A2B,
                extra: None,
            })
        );
    }

    /// A burst decoded mid-call is handed out by `take_mdc` once and only
    /// once; the next carrier re-arms it. The end-of-call summary still
    /// carries the packet — that stays the persistent record.
    #[test]
    fn take_mdc_reports_a_burst_once_and_rearms_on_the_next_carrier() {
        let burst = crate::mdc::burst(&[[0x01, 0x00, 0x1A, 0x2B]], false);
        let mut rx = receiver();
        let mut src = Source::new();
        for _ in 0..30 {
            let b = src.fm(BLOCK, 1000.0, 2500.0, 0.5);
            rx.process(&b, 20.0);
        }
        assert!(rx.in_call(), "voice should have opened the call");
        let mut pos = 0;
        for _ in 0..16 {
            let b = src.waveform(BLOCK, &burst, &mut pos, 750.0, 0.5);
            rx.process(&b, 20.0);
        }
        let pkt = rx.take_mdc().expect("the burst should be reported");
        assert_eq!(pkt.unit_id, 0x1A2B);
        assert_eq!(rx.take_mdc(), None, "a second take must not repeat it");

        let summary = drain_to_end(&mut rx, &mut src);
        assert_eq!(
            summary.mdc,
            Some(pkt.clone()),
            "taking the live packet must not cost the summary"
        );
        assert_eq!(rx.take_mdc(), None, "an ended call has nothing new");

        // A new carrier resets the flag: the same radio keying up again
        // reports again, once.
        let mut pos = 0;
        for _ in 0..16 {
            let b = src.waveform(BLOCK, &burst, &mut pos, 750.0, 0.5);
            rx.process(&b, 20.0);
        }
        assert_eq!(rx.take_mdc(), Some(pkt));
        assert_eq!(rx.take_mdc(), None);
    }

    /// An MDC-1200 PTT-ID burst right at key-up (before any separate voice)
    /// must be decoded even as carrier squelch is opening.
    #[test]
    fn an_mdc_ptt_id_at_call_onset_is_reported() {
        let burst = crate::mdc::burst(&[[0x01, 0x00, 0x1A, 0x2B]], false);
        let mut rx = receiver();
        let mut src = Source::new();
        let mut pos = 0;
        // The burst arrives on the first blocks of carrier
        for _ in 0..16 {
            let b = src.waveform(BLOCK, &burst, &mut pos, 750.0, 0.5);
            rx.process(&b, 20.0);
        }
        // Then some voice to continue the call
        for _ in 0..20 {
            let b = src.fm(BLOCK, 1000.0, 2500.0, 0.5);
            rx.process(&b, 20.0);
        }
        let summary = drain_to_end(&mut rx, &mut src);
        assert_eq!(
            summary.mdc,
            Some(crate::mdc::MdcPacket {
                op: 0x01,
                arg: 0x00,
                unit_id: 0x1A2B,
                extra: None,
            })
        );
    }

    /// An MDC-1200 PTT-ID burst on a channel expecting CTCSS tone squelch.
    #[test]
    fn an_mdc_ptt_id_with_ctcss_is_reported() {
        let spec = ChannelSpec::new("test", CHAN).with_code(SquelchCode::Ctcss(192.8));
        let mut rx = ChannelReceiver::new(spec, FS_IN, CENTER);
        let mut src = Source::new();
        let burst = crate::mdc::burst(&[[0x01, 0x00, 0x1A, 0x2B]], false);
        let mut pos = 0;
        // Preamble & MDC burst with CTCSS
        for _ in 0..20 {
            let b = src.waveform(BLOCK, &burst, &mut pos, 750.0, 0.5);
            rx.process(&b, 20.0);
        }
        for _ in 0..60 {
            let b = src.fm(BLOCK, 192.8, 500.0, 0.5);
            rx.process(&b, 20.0);
        }
        let summary = drain_to_end(&mut rx, &mut src);
        assert_eq!(
            summary.mdc,
            Some(crate::mdc::MdcPacket {
                op: 0x01,
                arg: 0x00,
                unit_id: 0x1A2B,
                extra: None,
            })
        );
    }

    /// A mistuned channel must say so. This is the diagnostic that tells a
    /// misconfigured frequency apart from a genuinely poor signal.
    #[test]
    fn the_summary_reports_how_far_off_frequency_the_call_was() {
        // Place the transmitter 2 kHz above where the channel is configured.
        struct Offset {
            k: u64,
            phase: f64,
        }
        impl Offset {
            fn fm(&mut self, n: usize, err_hz: f64) -> Vec<Complex32> {
                let offset = CHAN - CENTER + err_hz;
                (0..n)
                    .map(|_| {
                        let t = self.k as f64 / FS_IN;
                        self.phase += TAU * 2500.0 * (TAU * 800.0 * t).sin() / FS_IN;
                        let a = TAU * offset * t + self.phase;
                        self.k += 1;
                        Complex32::new(0.5 * a.cos() as f32, 0.5 * a.sin() as f32)
                    })
                    .collect()
            }
        }
        let mut rx = receiver();
        let mut src = Offset { k: 0, phase: 0.0 };
        for _ in 0..40 {
            let b = src.fm(BLOCK, 2000.0);
            rx.process(&b, 20.0);
        }
        let mut quiet = Source::new();
        let summary = drain_to_end(&mut rx, &mut quiet);
        assert!(
            (summary.freq_error_hz - 2000.0).abs() < 250.0,
            "reported {:.0} Hz of error, expected about 2000",
            summary.freq_error_hz
        );
    }

    /// A span-detector spike with no quieted carrier is a hop/retune
    /// transient, not a call. Opening on RF energy alone used to write
    /// pre-roll + hang (~2.8 s) of static at 0 % voiced.
    #[test]
    fn rf_without_quieting_does_not_start_a_call() {
        let mut rx = receiver();
        let mut src = Source::new();
        for _ in 0..40 {
            let noise = src.noise(BLOCK);
            assert!(
                rx.process(&noise, 25.0).is_none(),
                "energy squelch without FM quieting started a call"
            );
        }
        assert!(!rx.in_call());
    }

    /// Noise alone must not manufacture calls.
    #[test]
    fn a_quiet_channel_produces_no_calls() {
        let mut rx = receiver();
        let mut src = Source::new();
        for _ in 0..20 {
            let quiet = src.silence(BLOCK);
            assert!(rx.process(&quiet, 1.0).is_none());
        }
        assert!(!rx.in_call());
    }
}
