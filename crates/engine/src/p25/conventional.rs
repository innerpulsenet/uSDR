//! Conventional P25 Phase 1 voice receiver.
//!
//! The shared C4FM front end and NID detector provide complete LDUs with P25
//! status symbols removed.  Each LDU contains nine full-rate IMBE codewords;
//! the intervening Hamming-protected hex words carry link control (LDU1) or
//! encryption synchronisation (LDU2).

use super::{C4FM_BANDWIDTH_HZ, C4fmFrontEnd, CHANNEL_RATE, Duid, Frame, FrameDetector, rs64};
use crate::afc::Afc;
use crate::channel::{AUDIO_RATE, CallEvent, CallSummary, DigitalCallTelemetry, ToneCode};
use crate::cqpsk::CqpskDemodulator;
use crate::dmr::Resampler8k;
use crate::leveler::Leveler;
use blip25_vocoder::fullrate::frame::{INFO_WIDTHS, decode_frame};
use blip25_vocoder::vocoder::{FrameStatus, Rate, Vocoder};
use num_complex::Complex32;
use scannerd_dsp::DecodeChain;

const HANG_S: f32 = 0.72;
const VOICE_OFFSETS: [usize; 9] = [0, 72, 164, 256, 348, 440, 532, 624, 712];
const SIGNAL_OFFSETS: [usize; 6] = [144, 236, 328, 420, 512, 604];

#[derive(Clone, Debug, PartialEq)]
pub struct P25Spec {
    pub name: String,
    pub freq_hz: f64,
    /// Optional NAC filter. `None` accepts the first valid NAC heard.
    pub nac: Option<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkControl {
    pub format: u8,
    pub manufacturer_id: u8,
    pub manufacturer: String,
    pub service_options: u8,
    pub group: bool,
    pub target_id: Option<u32>,
    pub source_id: Option<u32>,
    pub protected: bool,
    pub emergency: bool,
    pub encrypted: bool,
    pub priority: u8,
    pub corrected_words: u8,
    pub unreliable_words: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncryptionSync {
    pub message_indicator: [u8; 9],
    pub algorithm_id: u8,
    pub algorithm: String,
    pub key_id: u16,
    pub encrypted: bool,
    pub corrected_words: u8,
    pub unreliable_words: u8,
}

/// A live voice-grant observed on the control channel: what the system just
/// told every subscriber to do. The scanner surface uses these to *follow*
/// traffic — multimon-ng prints them, dsd-fme tunes to them; here they are
/// surfaced so the application layer can log or chase them.
#[derive(Clone, Debug, PartialEq)]
pub struct TrunkGrant {
    pub talkgroup: u32,
    /// Group call when true, individual otherwise.
    pub group: bool,
    pub source: u32,
    /// Absolute frequency the grant points at, via the learned band plan.
    pub freq_hz: f64,
}

pub struct P25ChannelReceiver {
    pub spec: P25Spec,
    chain: DecodeChain,
    afc: Afc,
    front: C4fmFrontEnd,
    detector: FrameDetector,
    cqpsk: CqpskDemodulator,
    cqpsk_detector: FrameDetector,
    vocoder: Vocoder,
    decryptor: Option<crate::crypto::VoiceDecryptor>,
    resampler: Resampler8k,
    leveler: Leveler,
    baseband: Vec<Complex32>,
    discriminator: Vec<f32>,
    cqpsk_symbols: Vec<f32>,
    out: Vec<f32>,
    in_call: bool,
    call_samples: usize,
    peak_snr_db: f32,
    hang_s: f32,
    nac: Option<u16>,
    link_control: Option<LinkControl>,
    encryption: Option<EncryptionSync>,
    call_decrypted: bool,
    offset_sum: f64,
    offset_n: usize,
    /// Baseband blocks since the C4FM path last produced a BCH-valid frame.
    /// Past a threshold the parallel linear CQPSK receiver is paused, because
    /// while C4FM is healthy its symbols are never used.
    cqpsk_idle_blocks: u32,
    /// Band plan learned from IDEN updates plus the most recent grant seen.
    plan: crate::p25::ChannelPlan,
    latest_grant: Option<TrunkGrant>,
    /// Grants waiting for the caller to pick up.
    grants: std::collections::VecDeque<TrunkGrant>,
}

impl P25ChannelReceiver {
    pub fn new(spec: P25Spec, fs_in: f64, span_center_hz: f64) -> Self {
        let mut out = Self::build(spec, DecodeChain::new(fs_in, C4FM_BANDWIDTH_HZ, CHANNEL_RATE));
        out.retune(span_center_hz);
        out
    }

    /// Build for input that is already the extracted channel: baseband
    /// centred on `spec.freq_hz` at `channel_fs` — a narrowband chain's
    /// output the receiver rides instead of re-extracting its own channel
    /// from the whole span. The chain collapses to decimation 1, and its
    /// rate grid makes `fs_out` exactly what the span-rate build would have
    /// produced (`span / round(span / CHANNEL_RATE)`), so every downstream
    /// symbol-timing constant is unchanged. The AFC base is zero: the
    /// carrier arrives at DC and only residual ±2.5 kHz tracking remains.
    pub fn new_on_channel(spec: P25Spec, channel_fs: f64) -> Self {
        Self::build(spec, DecodeChain::new(channel_fs, C4FM_BANDWIDTH_HZ, CHANNEL_RATE))
    }

    fn build(spec: P25Spec, chain: DecodeChain) -> Self {
        let fs_chan = chain.fs_out();
        Self {
            chain,
            afc: Afc::new(0.0, 2_500.0),
            front: C4fmFrontEnd::new(fs_chan),
            detector: FrameDetector::with_payload(),
            cqpsk: CqpskDemodulator::new(fs_chan, 4_800.0).with_equalizer(7, 0.02),
            cqpsk_detector: FrameDetector::with_payload(),
            vocoder: Vocoder::new(Rate::FullRate4400x4400),
            decryptor: None,
            resampler: Resampler8k::new(),
            leveler: Leveler::new(AUDIO_RATE),
            baseband: Vec::new(),
            discriminator: Vec::new(),
            cqpsk_symbols: Vec::new(),
            out: Vec::new(),
            in_call: false,
            call_samples: 0,
            peak_snr_db: f32::NEG_INFINITY,
            hang_s: 0.0,
            nac: None,
            link_control: None,
            encryption: None,
            call_decrypted: false,
            offset_sum: 0.0,
            offset_n: 0,
            cqpsk_idle_blocks: 0,
            plan: crate::p25::ChannelPlan::new(),
            latest_grant: None,
            grants: std::collections::VecDeque::new(),
            spec,
        }
    }

    pub fn retune(&mut self, span_center_hz: f64) {
        self.afc.set_base(self.spec.freq_hz - span_center_hz);
        self.chain.set_offset(self.afc.mix_hz());
    }

    pub fn reset(&mut self) {
        self.detector = FrameDetector::with_payload();
        self.cqpsk.reset();
        self.cqpsk_detector = FrameDetector::with_payload();
        self.vocoder = Vocoder::new(Rate::FullRate4400x4400);
        self.decryptor = None;
        self.resampler = Resampler8k::new();
        self.leveler.reset();
        self.out.clear();
        self.in_call = false;
        self.call_samples = 0;
        self.peak_snr_db = f32::NEG_INFINITY;
        self.hang_s = 0.0;
        self.nac = None;
        self.link_control = None;
        self.encryption = None;
        self.call_decrypted = false;
        self.offset_sum = 0.0;
        self.offset_n = 0;
        self.cqpsk_idle_blocks = 0;
          self.plan = crate::p25::ChannelPlan::new();
        self.latest_grant = None;
        self.grants.clear();
    }

    pub fn audio(&self) -> &[f32] {
        &self.out
    }

    pub fn audio_rate(&self) -> f64 {
        AUDIO_RATE
    }

    pub fn in_call(&self) -> bool {
        self.in_call
    }

    pub fn locked(&self) -> bool {
        self.nac.is_some()
    }

    /// Average carrier offset from the configured channel frequency while
    /// synced, in Hz. `None` until enough frames have accumulated to trust
    /// the average. This is what lets a scanner re-centre a candidate the
    /// crystal residual left off-frequency.
    pub fn carrier_offset_hz(&self) -> Option<f64> {
        (self.offset_n > 32).then(|| self.offset_sum / self.offset_n as f64)
    }

    /// `(encrypted, decrypted)` once the air interface has supplied enough
    /// signalling to make a safe audio decision. `None` deliberately means
    /// mute: absence of an LC/ESS is not evidence that a call is clear.
    fn voice_security(&self) -> Option<(bool, bool)> {
        if self.link_control.is_none() && self.encryption.is_none() {
            return None;
        }
        let encrypted = self
            .link_control
            .as_ref()
            .is_some_and(|lc| lc.encrypted || lc.protected)
            || self.encryption.as_ref().is_some_and(|ess| ess.encrypted);
        Some((encrypted, encrypted && self.decryptor.is_some()))
    }

    /// End an in-progress trunked call when the control-channel grant expires.
    pub fn finish(&mut self) -> Option<CallEvent> {
        self.end_call()
    }

    pub fn process(&mut self, iq: &[Complex32], snr_db: f32) -> Option<CallEvent> {
        self.out.clear();
        self.chain.process(iq, &mut self.baseband);
        if self.baseband.is_empty() {
            return None;
        }
        self.front.process(&self.baseband, &mut self.discriminator);
        let mut frames = self.detector.push(&self.discriminator);
        // The linear CQPSK path is a second, parallel receiver. While the
        // C4FM path is producing BCH-valid frames it can only burn half the
        // per-block DSP applying symbols nobody reads — so pause it while
        // C4FM is healthy and re-arm the moment that stops being true.
        let c4fm_healthy = frames.iter().any(|frame| frame.bch_ok && frame.duid.is_known());
        if c4fm_healthy {
            self.cqpsk_idle_blocks = 0;
        } else {
            self.cqpsk_idle_blocks = self.cqpsk_idle_blocks.saturating_add(1);
        }
        // ~1 s of baseband without a valid C4FM frame: long enough to ride
        // out damaged frames, short enough that a true CQPSK signal takes
        // over quickly.
        const CQPSK_IDLE_LIMIT: u32 = 50;
        let mut cqpsk_frames = Vec::new();
        if self.cqpsk_idle_blocks < CQPSK_IDLE_LIMIT {
            self.cqpsk.process(&self.baseband, &mut self.cqpsk_symbols);
            cqpsk_frames = self.cqpsk_detector.push(&self.cqpsk_symbols);
        }
        // A clean C4FM path wins when both happen to correlate. Otherwise the
        // linear CQPSK path supplies the exact same NID/payload contract to
        // the rest of the receiver.
        if !frames.iter().any(|frame| frame.bch_ok && frame.duid.is_known()) {
            frames.extend(cqpsk_frames);
        }
        let mut event = None;
        let mut heard_voice = false;
        for frame in frames {
            if !frame.bch_ok || self.spec.nac.is_some_and(|nac| nac != frame.nac) {
                continue;
            }
            self.nac = Some(frame.nac);
            self.afc.observe(frame.offset_hz, true);
            self.chain.set_offset(self.afc.mix_hz());
            self.offset_sum += f64::from(self.afc.correction_hz() + frame.offset_hz);
            self.offset_n += 1;
            match frame.duid {
                Duid::Header => {
                    if let Some(header) = decode_header_encryption(&frame) {
                        self.encryption = Some(header);
                        self.decryptor = None;
                    }
                }
                Duid::Ldu1 | Duid::Ldu2 => {
                    heard_voice = true;
                    if frame.duid == Duid::Ldu1 {
                        // A damaged LC must not erase good call state from a
                        // preceding LDU1. In particular, losing the protected
                        // service-options bit here would briefly unmute
                        // encrypted IMBE and later file the call as clear.
                        if let Some(link_control) = decode_link_control(&frame) {
                            self.link_control = Some(link_control);
                        }
                        self.decryptor = self.encryption.as_ref().and_then(|ess| {
                            crate::crypto::VoiceDecryptor::for_p25_phase1(
                                ess.algorithm_id,
                                ess.key_id,
                                &ess.message_indicator,
                                0,
                            )
                        });
                    } else {
                        if let Some(encryption) = decode_encryption_sync(&frame) {
                            self.encryption = Some(encryption);
                            self.decryptor = self.encryption.as_ref().and_then(|ess| {
                                crate::crypto::VoiceDecryptor::for_p25_phase1(
                                    ess.algorithm_id,
                                    ess.key_id,
                                    &ess.message_indicator,
                                    9,
                                )
                            });
                        }
                    }
                    // Fail closed until an LC or ESS has actually established
                    // whether this call is clear. Treating missing signalling
                    // as clear sent encrypted IMBE to the vocoder during late
                    // entry and created the garbled recordings seen on NMPD.
                    let Some((encrypted, decrypted)) = self.voice_security() else {
                        continue;
                    };
                    self.call_decrypted |= decrypted;
                    let mut pcm = if !encrypted || decrypted {
                        self.decode_voice(&frame)
                    } else {
                        vec![0i16; 9 * 160]
                    };
                    let mut audio = self.resampler.process(&pcm);
                    self.leveler.process(&mut audio);
                    if !self.in_call {
                        self.in_call = true;
                        self.call_samples = 0;
                        self.peak_snr_db = snr_db;
                        event = Some(CallEvent::Started);
                    }
                    self.call_samples += audio.len();
                    self.out.append(&mut audio);
                    pcm.clear();
                    self.hang_s = 0.0;
                    self.peak_snr_db = self.peak_snr_db.max(snr_db);
                }
                Duid::Terminator | Duid::TerminatorLc => {
                    event = self.end_call().or(event);
                }
                Duid::Tsdu => self.observe_trunking(&frame),
                _ => {}
            }
        }

        if self.in_call && !heard_voice {
            self.hang_s += self.baseband.len() as f32 / self.chain.fs_out() as f32;
            if self.hang_s >= HANG_S {
                event = self.end_call().or(event);
            }
        }
        event
    }

    /// Test hook: feed a synthetic TSDU frame through the trunking observer
    /// without driving the whole DSP chain.
    #[cfg(test)]
    fn observe_trunking_for_test(&mut self, frame: &Frame) {
        self.observe_trunking(frame);
    }

    /// Learn what the control channel is telling subscribers to do. IDEN
    /// updates feed the band plan; grants become follow requests the
    /// application layer can act on.
    fn observe_trunking(&mut self, frame: &Frame) {
        for block in &frame.tsbks {
            if !block.crc_ok {
                continue;
            }
            let event = block.event();
            if matches!(event, crate::p25::TsbkEvent::IdenUpdate { .. }) {
                self.plan.observe(&event);
                continue;
            }
            let (talkgroup, group, source, channel) = match event {
                crate::p25::TsbkEvent::GroupVoiceGrant {
                    talkgroup, source, channel, ..
                } => (u32::from(talkgroup), true, source, channel),
                crate::p25::TsbkEvent::IndividualVoiceGrant {
                    target, source, channel, ..
                } => (target, false, source, channel),
                _ => continue,
            };
            // Without a band plan the grant cannot name a frequency; the
            // raw channel is still worth logging but not worth following.
            let Some(freq_hz) = self.plan.frequency(channel) else {
                continue;
            };
            let grant = TrunkGrant {
                talkgroup,
                group,
                source,
                freq_hz,
            };
            self.latest_grant = Some(grant.clone());
            if self.grants.len() >= 8 {
                self.grants.pop_front();
            }
            self.grants.push_back(grant);
        }
    }

    /// Grants observed since the last call to this method, in order.
    pub fn pending_grants(&mut self) -> Vec<TrunkGrant> {
        self.grants.drain(..).collect()
    }

    /// The most recent grant observed on the control channel, for telemetry.
    pub fn latest_grant(&self) -> Option<&TrunkGrant> {
        self.latest_grant.as_ref()
    }

    fn decode_voice(&mut self, frame: &Frame) -> Vec<i16> {
        let mut pcm = Vec::with_capacity(9 * 160);
        for &start in &VOICE_OFFSETS {
            let Some(dibits) = frame.payload.get(start..start + 72) else {
                pcm.extend([0i16; 160]);
                continue;
            };
            let mut dibit_frame = [0u8; 72];
            for (i, &dibit) in dibits.iter().enumerate() {
                dibit_frame[i] = dibit & 3;
            }
            let recovered = decode_frame(&dibit_frame);
            let mut info_bytes = pack_imbe_info(&recovered.info);
            if let Some(decryptor) = &mut self.decryptor {
                decryptor.apply_imbe88(&mut info_bytes);
            }
            let info = unpack_imbe_info(&info_bytes);
            let errors = u32::from(recovered.error_total());
            match self
                .vocoder
                .decode_info(&info, FrameStatus::new(errors, false))
            {
                Ok(samples) => pcm.extend(samples),
                Err(_) => pcm.extend([0i16; 160]),
            }
        }
        pcm
    }

    fn end_call(&mut self) -> Option<CallEvent> {
        if !self.in_call {
            return None;
        }
        self.in_call = false;
        self.hang_s = 0.0;
        let nac = self.nac;
        let lc = self.link_control.take();
        let ess = self.encryption.take();
        self.decryptor = None;
        let encrypted = lc.as_ref().is_some_and(|v| v.encrypted || v.protected)
            || ess.as_ref().is_some_and(|v| v.encrypted);
        let decrypted = self.call_decrypted;
        let summary = CallSummary {
            audio_samples: self.call_samples,
            peak_snr_db: self.peak_snr_db,
            tone: nac.map(|nac| ToneCode::P25 { nac }),
            mdc: None,
            freq_error_hz: if self.offset_n > 0 {
                (self.offset_sum / self.offset_n as f64) as f32
            } else {
                0.0
            },
            voiced_fraction: 1.0,
            digital: Some(DigitalCallTelemetry {
                protocol: nac
                    .map(|n| format!("P25 Phase 1 NAC ${n:03X}"))
                    .unwrap_or_else(|| "P25 Phase 1".into()),
                color_code: None,
                slot: None,
                source_id: lc.as_ref().and_then(|v| v.source_id),
                target_id: lc.as_ref().and_then(|v| v.target_id),
                group: lc.as_ref().map(|v| v.group),
                manufacturer: lc.as_ref().map(|v| v.manufacturer.clone()),
                service_options: lc.as_ref().map(|v| v.service_options),
                emergency: lc.as_ref().is_some_and(|v| v.emergency),
                encrypted,
                decrypted,
                algorithm_id: ess.as_ref().map(|v| v.algorithm_id),
                key_id: ess.as_ref().map(|v| v.key_id),
                talker_alias: None,
                bit_error_pct: None,
            }),
        };
        self.call_samples = 0;
        self.peak_snr_db = f32::NEG_INFINITY;
        self.offset_sum = 0.0;
        self.offset_n = 0;
        self.call_decrypted = false;
        Some(CallEvent::Ended(summary))
    }
}

fn pack_imbe_info(info: &[u16; 8]) -> [u8; 11] {
    let mut out = [0u8; 11];
    let mut at = 0usize;
    for (value, width) in info.iter().zip(INFO_WIDTHS) {
        for shift in (0..usize::from(width)).rev() {
            out[at / 8] |= (((value >> shift) & 1) as u8) << (7 - at % 8);
            at += 1;
        }
    }
    out
}

fn unpack_imbe_info(bytes: &[u8; 11]) -> [u16; 8] {
    let mut out = [0u16; 8];
    let mut at = 0usize;
    for (value, width) in out.iter_mut().zip(INFO_WIDTHS) {
        for _ in 0..width {
            *value = (*value << 1) | u16::from((bytes[at / 8] >> (7 - at % 8)) & 1);
            at += 1;
        }
    }
    out
}

pub fn decode_link_control(frame: &Frame) -> Option<LinkControl> {
    if frame.duid != Duid::Ldu1 || frame.payload.len() < 624 {
        return None;
    }
    let (words, corrected, unreliable) = signalling_words(&frame.payload, 12)?;
    let bits: Vec<u8> = words[..12]
        .iter()
        .flat_map(|word| (0..6).rev().map(move |bit| (word >> bit) & 1))
        .collect();
    let bytes = pack_bits(&bits);
    let format = bytes[0];
    let manufacturer_id = bytes[1];
    let service_options = bytes[2];
    let (group, target_id, source_id) = match format & 0x3f {
        0x00 => (
            true,
            Some(u32::from(u16::from_be_bytes([bytes[4], bytes[5]]))),
            Some(u24(&bytes[6..9])),
        ),
        0x03 => (false, Some(u24(&bytes[3..6])), Some(u24(&bytes[6..9]))),
        _ => (false, None, None),
    };
    Some(LinkControl {
        format,
        manufacturer_id,
        manufacturer: manufacturer_name(manufacturer_id).into(),
        service_options,
        group,
        target_id,
        source_id,
        protected: format & 0x80 != 0,
        emergency: service_options & 0x80 != 0,
        encrypted: service_options & 0x40 != 0,
        priority: service_options & 7,
        corrected_words: corrected,
        unreliable_words: unreliable,
    })
}

pub fn decode_encryption_sync(frame: &Frame) -> Option<EncryptionSync> {
    if frame.duid != Duid::Ldu2 || frame.payload.len() < 624 {
        return None;
    }
    let (words, corrected, unreliable) = signalling_words(&frame.payload, 16)?;
    let bits: Vec<u8> = words[..16]
        .iter()
        .flat_map(|word| (0..6).rev().map(move |bit| (word >> bit) & 1))
        .collect();
    let bytes = pack_bits(&bits);
    let mut message_indicator = [0u8; 9];
    message_indicator.copy_from_slice(&bytes[..9]);
    let algorithm_id = bytes[9];
    Some(EncryptionSync {
        message_indicator,
        algorithm_id,
        algorithm: algorithm_name(algorithm_id).into(),
        key_id: u16::from_be_bytes([bytes[10], bytes[11]]),
        encrypted: algorithm_id != 0 && algorithm_id != 0x80,
        corrected_words: corrected,
        unreliable_words: unreliable,
    })
}

/// Decode the HDU's RS(36,20) encryption header. Each GF(64) symbol is sent
/// as six data bits plus twelve shortened extended-Golay parity bits; both the
/// information and parity symbol groups appear in reverse RS order on air.
pub fn decode_header_encryption(frame: &Frame) -> Option<EncryptionSync> {
    if frame.duid != Duid::Header || frame.payload.len() < 36 * 9 {
        return None;
    }
    let mut air_words = Vec::with_capacity(36);
    let mut corrected = 0u8;
    let mut unreliable = 0u8;
    for chunk in frame.payload[..36 * 9].chunks_exact(9) {
        let mut received = 0u32;
        for &dibit in chunk {
            received = (received << 2) | u32::from(dibit & 3);
        }
        let mut best = (u32::MAX, 0u8);
        for value in 0..64u8 {
            let distance = (received ^ hdu_golay_word(value)).count_ones();
            if distance < best.0 {
                best = (distance, value);
            }
        }
        corrected = corrected.saturating_add(best.0.min(u32::from(u8::MAX)) as u8);
        unreliable = unreliable.saturating_add(u8::from(best.0 > 3));
        air_words.push(best.1);
    }
    // HDU Golay words use the same systematic information-then-parity order
    // as the reference encoder and decoder.
    let mut words = air_words;
    corrected = corrected.saturating_add(rs64::correct(&mut words, 20)?);
    let bits: Vec<u8> = words[..20]
        .iter()
        .flat_map(|word| (0..6).rev().map(move |bit| (word >> bit) & 1))
        .collect();
    let bytes = pack_bits(&bits);
    let mut message_indicator = [0u8; 9];
    message_indicator.copy_from_slice(&bytes[..9]);
    let algorithm_id = bytes[10];
    Some(EncryptionSync {
        message_indicator,
        algorithm_id,
        algorithm: algorithm_name(algorithm_id).into(),
        key_id: u16::from_be_bytes([bytes[11], bytes[12]]),
        encrypted: algorithm_id != 0 && algorithm_id != 0x80,
        corrected_words: corrected,
        unreliable_words: unreliable,
    })
}

fn hdu_golay_word(value: u8) -> u32 {
    // This is the systematic extended Golay representation used by the P25
    // HDU: the six received bits occupy the upper half of the 12-bit data
    // field, and the twelve transmitted parity bits are sent least-significant
    // first. POLY is x^11+x^9+x^7+x^6+x^5+x+1.
    let reversed = value.reverse_bits() >> 2;
    let mut work = u32::from(reversed) << 6;
    let data = work;
    for _ in 0..12 {
        if work & 1 != 0 {
            work ^= 0x0ae3;
        }
        work >>= 1;
    }
    let mut codeword = (work << 12) | data;
    if codeword.count_ones() & 1 != 0 {
        codeword ^= 1 << 23;
    }
    let mut air = u32::from(value) << 12;
    for bit in 0..12 {
        air |= ((codeword >> (12 + bit)) & 1) << (11 - bit);
    }
    air
}

fn signalling_words(payload: &[u8], information_symbols: usize) -> Option<(Vec<u8>, u8, u8)> {
    let mut raw = Vec::with_capacity(240);
    for &start in &SIGNAL_OFFSETS {
        for &d in payload.get(start..start + 20)? {
            raw.push((d >> 1) & 1);
            raw.push(d & 1);
        }
    }
    let mut words = Vec::with_capacity(24);
    let mut corrected = 0u8;
    let mut unreliable = 0u8;
    for chunk in raw.chunks_exact(10).take(24) {
        let mut received = 0u16;
        for &bit in chunk {
            received = (received << 1) | u16::from(bit);
        }
        let mut best = (u32::MAX, 0u8);
        for data in 0..64u8 {
            let code = (u16::from(data) << 4) | u16::from(hamming_parity(data));
            let distance = (received ^ code).count_ones();
            if distance < best.0 {
                best = (distance, data);
            }
        }
        corrected += u8::from(best.0 == 1);
        unreliable += u8::from(best.0 > 1);
        words.push(best.1);
    }
    // The 24 Hamming codewords are transmitted in the same systematic order
    // consumed by the P25 RS decoder: information symbols first, then parity.
    // Reversing either group made real LDU2 ESS words fail RS validation and
    // occasionally miscorrect into bogus ALGIDs such as 0x08 and 0x30.
    let rs_corrected = rs64::correct_24(&mut words, information_symbols)?;
    corrected = corrected.saturating_add(rs_corrected);
    Some((words, corrected, unreliable))
}

fn hamming_parity(data: u8) -> u8 {
    const MASKS: [u8; 4] = [0b111001, 0b110101, 0b101110, 0b011110];
    MASKS.iter().fold(0u8, |out, mask| {
        (out << 1) | ((data & mask).count_ones() as u8 & 1)
    })
}

fn pack_bits(bits: &[u8]) -> Vec<u8> {
    bits.chunks(8)
        .map(|chunk| chunk.iter().fold(0u8, |v, bit| (v << 1) | bit))
        .collect()
}

fn u24(bytes: &[u8]) -> u32 {
    (u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2])
}

fn manufacturer_name(id: u8) -> &'static str {
    match id {
        0x00 => "TIA standard",
        0x01 => "TIA standard (legacy)",
        0x10 => "Motorola",
        0x18 => "EADS",
        0x20 => "Kenwood",
        0x40 => "Harris",
        0x68 => "Hytera",
        _ => "Unknown P25 manufacturer",
    }
}

pub fn algorithm_name(id: u8) -> &'static str {
    match id {
        0x00 | 0x80 => "Clear",
        0x81 => "DES-OFB",
        0x83 => "3-key Triple DES",
        0x84 => "AES-256-OFB",
        0x85 => "AES-128-ECB",
        0x88 => "AES-CBC",
        0x89 => "AES-128-OFB",
        0x9f => "DES-XL",
        0xaa => "Motorola ADP",
        _ => "Unknown P25 algorithm",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A control channel that announces a band plan and then grants a
    /// talkgroup must yield a followable frequency — this is the whole point
    /// of decoding a trunked control channel.
    #[test]
    fn tsdu_grants_surface_as_followable_frequencies() {
        let mut rx = P25ChannelReceiver::new(
            P25Spec { name: "t".into(), freq_hz: 851_000_000.0, nac: None },
            960_000.0,
            851_000_000.0,
        );
        // IDEN update (opcode 0x34): band 1, base 851_006_250 Hz in 5 Hz
        // units, spacing 12.5 kHz in 125 Hz units, FDMA.
        let base_units = 851_006_250u64 / 5;
        let spacing_units = 12_500u64 / 125;
        let iden = crate::p25::tsbk::Tsbk {
            last: true,
            protected: false,
            opcode: 0x34,
            mfid: 0,
            args: (1u64 << 60) | (spacing_units << 32) | base_units,
            crc_ok: true,
        };
        rx.observe_trunking_for_test(&crate::p25::Frame {
            nac: 0x293,
            duid: crate::p25::Duid::Tsdu,
            correlation: 1.0,
            deviation_hz: 1800.0,
            bch_ok: true,
            corrected_bits: 0,
            offset_hz: 0.0,
            tsbks: vec![iden],
            payload: Vec::new(),
        });
        // Grant: opcode 0x00, options C3, channel 0x1234 -> band 1 ch 0x234.
        let grant = crate::p25::tsbk::Tsbk {
            last: true,
            protected: false,
            opcode: 0x00,
            mfid: 0,
            args: (0xC3u64 << 56) | (0x1234u64 << 40) | (0x0426u64 << 24) | 0x00_1F41,
            crc_ok: true,
        };
        rx.observe_trunking_for_test(&crate::p25::Frame {
            nac: 0x293,
            duid: crate::p25::Duid::Tsdu,
            correlation: 1.0,
            deviation_hz: 1800.0,
            bch_ok: true,
            corrected_bits: 0,
            offset_hz: 0.0,
            tsbks: vec![grant],
            payload: Vec::new(),
        });

        let pending = rx.pending_grants();
        assert_eq!(pending.len(), 1);
        let g = &pending[0];
        assert_eq!(g.talkgroup, 0x426);
        assert!(g.group);
        // Band 1, carrier 0x234 = 564 × 12.5 kHz above base... the IDEN args
        // encode base in 5 Hz units and spacing in 125 Hz units; whatever the
        // plan computed, it must be the SAME mapping ChannelPlan gives direct.
        let plan_expected = {
            let mut p = crate::p25::ChannelPlan::new();
            // mirror of the IDEN block above
            p.observe(&crate::p25::TsbkEvent::IdenUpdate {
                id: 1,
                base_hz: 851_006_250.0,
                spacing_hz: 12_500.0,
                tdma: false,
            });
            p.frequency(0x1234).unwrap()
        };
        assert_eq!(g.freq_hz, plan_expected);
    }


    fn ldu_with_signalling(duid: Duid, bytes: &[u8], information_symbols: usize) -> Frame {
        let bits: Vec<u8> = bytes
            .iter()
            .flat_map(|byte| (0..8).rev().map(move |bit| (byte >> bit) & 1))
            .collect();
        let information: Vec<u8> = bits
            .chunks(6)
            .map(|chunk| chunk.iter().fold(0u8, |word, bit| (word << 1) | bit))
            .collect();
        assert_eq!(information.len(), information_symbols);
        let air_words = rs64::encode_for_test(&information, 24 - information_symbols);
        assert_eq!(&air_words[..information_symbols], information);
        let signalling: Vec<u8> = air_words
            .iter()
            .flat_map(|word| {
                let encoded = (u16::from(*word) << 4) | u16::from(hamming_parity(*word));
                (0..5)
                    .rev()
                    .map(move |dibit| ((encoded >> (dibit * 2)) & 3) as u8)
            })
            .collect();
        let mut payload = vec![0u8; 784];
        for (chunk, &start) in signalling.chunks_exact(20).zip(&SIGNAL_OFFSETS) {
            payload[start..start + 20].copy_from_slice(chunk);
        }
        Frame {
            nac: 0x290,
            duid,
            correlation: 1.0,
            deviation_hz: 1800.0,
            bch_ok: true,
            corrected_bits: 0,
            offset_hz: 0.0,
            tsbks: Vec::new(),
            payload,
        }
    }

    #[test]
    fn hamming_words_survive_one_bit() {
        for data in 0..64u8 {
            let code = (u16::from(data) << 4) | u16::from(hamming_parity(data));
            for bit in 0..10 {
                let damaged = code ^ (1 << bit);
                let mut raw = Vec::new();
                for i in (0..10).rev() {
                    raw.push(((damaged >> i) & 1) as u8);
                }
                let mut best = (u32::MAX, 0);
                for candidate in 0..64u8 {
                    let c = (u16::from(candidate) << 4) | u16::from(hamming_parity(candidate));
                    best = best.min(((damaged ^ c).count_ones(), candidate));
                }
                assert_eq!(best.1, data, "data {data}, bit {bit}, {raw:?}");
            }
        }
    }

    #[test]
    fn algorithm_labels_clear_and_aes() {
        assert_eq!(algorithm_name(0x80), "Clear");
        assert_eq!(algorithm_name(0x84), "AES-256-OFB");
    }

    #[test]
    fn missing_security_signalling_never_defaults_to_clear_audio() {
        let mut receiver = P25ChannelReceiver::new(
            P25Spec {
                name: "test".into(),
                freq_hz: 154_860_000.0,
                nac: Some(0x290),
            },
            48_000.0,
            154_860_000.0,
        );
        assert_eq!(receiver.voice_security(), None);
        receiver.encryption = Some(EncryptionSync {
            message_indicator: [0; 9],
            algorithm_id: 0x84,
            algorithm: "AES-256-OFB".into(),
            key_id: 0x1234,
            encrypted: true,
            corrected_words: 0,
            unreliable_words: 0,
        });
        assert_eq!(receiver.voice_security(), Some((true, false)));
    }

    #[test]
    fn imbe_information_packing_round_trips_all_field_widths() {
        let info = [0xabc, 0x123, 0xfff, 0x800, 0x7ff, 0x456, 0x001, 0x7f];
        assert_eq!(unpack_imbe_info(&pack_imbe_info(&info)), info);
    }

    #[test]
    fn ldu1_reference_air_order_decodes_encrypted_service_options() {
        let frame = ldu_with_signalling(
            Duid::Ldu1,
            &[0x00, 0x00, 0xc5, 0x00, 0x12, 0x34, 0x56, 0x78, 0x9a],
            12,
        );
        let lc = decode_link_control(&frame).expect("valid LDU1 LC");
        assert!(lc.encrypted);
        assert!(lc.emergency);
        assert_eq!(lc.priority, 5);
        assert_eq!(lc.target_id, Some(0x1234));
        assert_eq!(lc.source_id, Some(0x56789a));
    }

    #[test]
    fn ldu2_reference_air_order_decodes_encryption_sync() {
        let frame = ldu_with_signalling(
            Duid::Ldu2,
            &[
                0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x00, 0x84, 0x12, 0x34,
            ],
            16,
        );
        let ess = decode_encryption_sync(&frame).expect("valid LDU2 ESS");
        assert_eq!(
            ess.message_indicator,
            [1, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0]
        );
        assert_eq!(ess.algorithm_id, 0x84);
        assert_eq!(ess.key_id, 0x1234);
        assert!(ess.encrypted);
    }

    #[test]
    fn hdu_seeds_phase_one_encryption_before_the_first_ldu() {
        let bytes = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x00, // MI
            0x10, // MFID
            0x84, // ALGID
            0x12, 0x34, // KID
            0x00, 0x03, // talkgroup
        ];
        let bits: Vec<u8> = bytes
            .iter()
            .flat_map(|byte| (0..8).rev().map(move |bit| (byte >> bit) & 1))
            .collect();
        let information: Vec<u8> = bits
            .chunks(6)
            .map(|chunk| chunk.iter().fold(0u8, |word, bit| (word << 1) | bit))
            .collect();
        let air_words = rs64::encode_for_test(&information, 16);
        let payload = air_words
            .iter()
            .flat_map(|word| {
                let encoded = hdu_golay_word(*word);
                (0..9)
                    .rev()
                    .map(move |dibit| ((encoded >> (dibit * 2)) & 3) as u8)
            })
            .collect();
        let frame = Frame {
            nac: 0x123,
            duid: Duid::Header,
            correlation: 1.0,
            deviation_hz: 1800.0,
            bch_ok: true,
            corrected_bits: 0,
            offset_hz: 0.0,
            tsbks: Vec::new(),
            payload,
        };
        let header = decode_header_encryption(&frame).expect("HDU");
        assert_eq!(header.message_indicator, bytes[..9]);
        assert_eq!(header.algorithm_id, 0x84);
        assert_eq!(header.key_id, 0x1234);
        assert!(header.encrypted);
    }
}
