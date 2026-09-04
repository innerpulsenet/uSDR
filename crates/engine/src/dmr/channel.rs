//! Conventional DMR channel: IQ in, calls and audio out.
//!
//! Same shape as the analog [`ChannelReceiver`](crate::channel::ChannelReceiver)
//! so the scanner can treat a DMR frequency as just another entry on the
//! conventional list. Call lifetime is a voice superframe (and the
//! terminator that closes it), not analog energy squelch.

use super::framer::{self, Burst, BurstKind, DmrReceiver};
use super::voice;
use super::{
    CHANNEL_RATE, DMR_BANDWIDTH_HZ, DataAssembler, DataPdu, DecodedData, EmbeddedLcAssembler,
    LinkControl, PrivacyHeader, Resampler8k, decode_data_pdu,
};
use crate::afc::Afc;
use crate::channel::{AUDIO_RATE, CallEvent, CallSummary, DigitalCallTelemetry, ToneCode};
use crate::leveler::Leveler;
use num_complex::Complex32;
use scannerd_dsp::DecodeChain;

/// Seconds without a voice burst before a call is closed.
///
/// Must be several TDMA slot periods (60 ms), not IQ-block counts: the
/// analog scanner delivers ~8 ms blocks, so a 12-block hang was 0.1 s and
/// chopped every transmission after the opening burst.
const HANG_S: f32 = 0.60;

/// How a conventional DMR monitor is configured.
#[derive(Clone, Debug, PartialEq)]
pub struct DmrSpec {
    pub name: String,
    pub freq_hz: f64,
    /// Accept only this colour code. `None` accepts any.
    pub color_code: Option<u8>,
    /// Accept only this timeslot (1 or 2). `None` follows the first active slot.
    pub slot: Option<u8>,
}

pub struct DmrChannelReceiver {
    pub spec: DmrSpec,
    chain: DecodeChain,
    afc: Afc,
    rx: DmrReceiver,
    vocoder: rmbe::Decoder,
    resampler: Resampler8k,
    leveler: Leveler,
    baseband: Vec<Complex32>,
    out: Vec<f32>,
    in_call: bool,
    call_samples: usize,
    peak_snr_db: f32,
    tone: Option<ToneCode>,
    offset_sum: f64,
    offset_n: usize,
    voice_error_rate: f32,
    hang_s: f32,
    /// Timeslot we are currently following, once a call has started.
    follow_slot: Option<u8>,
    /// Voice audio held until a configured colour code is confirmed.
    pending: Vec<f32>,
    link_control: Option<LinkControl>,
    privacy: Option<PrivacyHeader>,
    decryptor: Option<crate::crypto::VoiceDecryptor>,
    embedded_lc: [EmbeddedLcAssembler; 3],
    data_assemblers: [DataAssembler; 3],
    decoded_data: Vec<DecodedData>,
}

impl DmrChannelReceiver {
    pub fn new(spec: DmrSpec, fs_in: f64, span_center_hz: f64) -> Self {
        let mut rx = Self::build(spec, DecodeChain::new(fs_in, DMR_BANDWIDTH_HZ, CHANNEL_RATE));
        rx.retune(span_center_hz);
        rx
    }

    /// Build for input that is already the extracted channel: baseband
    /// centred on `spec.freq_hz` at `channel_fs` — a narrowband chain's
    /// output the receiver rides instead of re-extracting its own channel
    /// from the whole span. The chain collapses to decimation 1, and its
    /// rate grid makes `fs_out` exactly what the span-rate build would have
    /// produced (`span / round(span / CHANNEL_RATE)`), so every downstream
    /// symbol-timing constant is unchanged. The AFC base is zero: the
    /// carrier arrives at DC and only residual ±2.5 kHz tracking remains.
    pub fn new_on_channel(spec: DmrSpec, channel_fs: f64) -> Self {
        Self::build(spec, DecodeChain::new(channel_fs, DMR_BANDWIDTH_HZ, CHANNEL_RATE))
    }

    fn build(spec: DmrSpec, chain: DecodeChain) -> Self {
        let fs_chan = chain.fs_out();
        Self {
            chain,
            afc: Afc::new(0.0, 2_500.0),
            rx: DmrReceiver::new(fs_chan),
            vocoder: rmbe::Decoder::new(),
            resampler: Resampler8k::new(),
            leveler: Leveler::new(AUDIO_RATE),
            baseband: Vec::new(),
            out: Vec::new(),
            in_call: false,
            call_samples: 0,
            peak_snr_db: f32::NEG_INFINITY,
            tone: None,
            offset_sum: 0.0,
            offset_n: 0,
            voice_error_rate: 0.0,
            hang_s: 0.0,
            follow_slot: None,
            pending: Vec::new(),
            link_control: None,
            privacy: None,
            decryptor: None,
            embedded_lc: std::array::from_fn(|_| EmbeddedLcAssembler::new()),
            data_assemblers: std::array::from_fn(|_| DataAssembler::new()),
            decoded_data: Vec::new(),
            spec,
        }
    }

    pub fn retune(&mut self, span_center_hz: f64) {
        self.afc.set_base(self.spec.freq_hz - span_center_hz);
        self.chain.set_offset(self.afc.mix_hz());
    }

    pub fn reset(&mut self) {
        self.in_call = false;
        self.call_samples = 0;
        self.peak_snr_db = f32::NEG_INFINITY;
        self.tone = None;
        self.offset_sum = 0.0;
        self.offset_n = 0;
        self.hang_s = 0.0;
        self.follow_slot = None;
        self.pending.clear();
        self.link_control = None;
        self.privacy = None;
        self.decryptor = None;
        for assembler in &mut self.embedded_lc {
            assembler.clear();
        }
        for assembler in &mut self.data_assemblers {
            assembler.reset();
        }
        self.decoded_data.clear();
        self.out.clear();
    }

    pub fn audio_rate(&self) -> f64 {
        AUDIO_RATE
    }

    pub fn audio(&self) -> &[f32] {
        &self.out
    }

    /// Application-layer data completed during the most recent `process`.
    pub fn decoded_data(&self) -> &[DecodedData] {
        &self.decoded_data
    }

    pub fn in_call(&self) -> bool {
        self.in_call
    }

    /// End an in-progress call when an external trunking controller releases
    /// the traffic channel. Conventional operation normally ends from a
    /// terminator or hang timer instead.
    pub fn finish(&mut self) -> Option<CallEvent> {
        self.end_call()
    }

    pub fn locked(&self) -> bool {
        self.rx.locked()
    }

    /// Latest hard-decision 4FSK quality estimate, in dB. Trunked traffic
    /// followers expose this so a bad LCN/frequency is distinguishable from
    /// a valid grant whose subscriber simply stopped transmitting.
    pub fn quality_db(&self) -> f32 {
        self.rx.quality_db()
    }

    /// Average carrier offset from the configured channel frequency while
    /// locked, in Hz. `None` until enough frames have accumulated to trust
    /// the average. This is what lets a scanner re-centre a candidate the
    /// crystal residual left off-frequency.
    pub fn carrier_offset_hz(&self) -> Option<f64> {
        (self.offset_n > 32).then(|| self.offset_sum / self.offset_n as f64)
    }

    /// Demodulate a block and advance the call state machine.
    ///
    /// `snr_db` is the span detector reading, used only for the call summary
    /// and the S-meter; gating is digital (sync / EMB / colour code).
    pub fn process(&mut self, iq: &[Complex32], snr_db: f32) -> Option<CallEvent> {
        self.out.clear();
        self.decoded_data.clear();
        self.chain.process(iq, &mut self.baseband);
        if self.baseband.is_empty() {
            return None;
        }

        let bursts = self.rx.process(&self.baseband);
        if self.rx.locked() {
            self.afc.observe(self.rx.offset_hz(), true);
            self.chain.set_offset(self.afc.mix_hz());
            self.offset_sum += f64::from(self.afc.correction_hz() + self.rx.offset_hz());
            self.offset_n += 1;
        }

        let mut event = None;
        let mut heard_voice = false;
        for burst in &bursts {
            if !self.accepts(burst) {
                continue;
            }
            let slot_index = burst
                .slot
                .map(usize::from)
                .filter(|slot| *slot <= 2)
                .unwrap_or(0);
            if let Some(link_control) = self.embedded_lc[slot_index].push(burst) {
                self.take_link_control(link_control);
            }
            if let Ok(pdu) = decode_data_pdu(burst) {
                match pdu {
                    DataPdu::LinkControl(link_control) => {
                        self.take_link_control(link_control);
                    }
                    DataPdu::PrivacyHeader(privacy) => {
                        let mi = decode_hex(&privacy.message_indicator);
                        self.decryptor = crate::crypto::VoiceDecryptor::for_call(
                            crate::crypto::Protocol::Dmr,
                            privacy.algorithm_id,
                            u16::from(privacy.key_id),
                            &mi,
                        );
                        self.privacy = Some(privacy);
                    }
                    DataPdu::DataHeader(header) => {
                        self.data_assemblers[slot_index].begin(header);
                    }
                    DataPdu::DataBlock(block) => {
                        if let Some(message) = self.data_assemblers[slot_index].push(&block) {
                            self.decoded_data.push(DecodedData::Message(message));
                        }
                    }
                    DataPdu::UnifiedSingleBlockData(data) => {
                        self.decoded_data
                            .push(DecodedData::UnifiedSingleBlock(data));
                    }
                }
            }
            match &burst.kind {
                BurstKind::Data {
                    slot_type: Some(st),
                    ..
                } if st.data_type == framer::DT_TERMINATOR_LC => {
                    // Only a terminator on the *same* known timeslot ends
                    // the call. A repeater's other slot is idle every frame;
                    // treating that (or a CACH-less idle) as end chopped
                    // every hit to one 60 ms voice burst (0.12 s).
                    if self.in_call && self.same_known_slot(burst.slot) {
                        event = self.end_call().or(event);
                    }
                }
                BurstKind::Voice { .. } => {
                    heard_voice = true;
                    let encrypted = burst.encrypted()
                        || self.link_control.as_ref().is_some_and(|lc| lc.encrypted)
                        || self.privacy.is_some();
                    // Preserve timing and create a metadata-bearing call for
                    // encrypted traffic, but never feed cipher text to rmbe.
                    let pcm = if encrypted && self.decryptor.is_none() {
                        vec![0.0; 3 * rmbe::SAMPLES_PER_FRAME * 2]
                    } else {
                        self.decode_voice(burst)
                    };
                    event = self.take_voice(burst, pcm, snr_db).or(event);
                }
                _ => {}
            }
        }

        if self.in_call {
            if heard_voice {
                self.hang_s = 0.0;
                self.peak_snr_db = self.peak_snr_db.max(snr_db).max(self.rx.quality_db());
            } else {
                let dt = self.baseband.len() as f32 / self.chain.fs_out() as f32;
                self.hang_s += dt;
                if self.hang_s >= HANG_S {
                    event = self.end_call().or(event);
                }
            }
        }

        event
    }

    fn accepts(&self, burst: &Burst) -> bool {
        if let Some(want) = self.spec.slot {
            if burst.slot.is_some_and(|s| s != want) {
                return false;
            }
        }
        if let Some(follow) = self.follow_slot {
            if burst.slot.is_some_and(|s| s != follow) {
                return false;
            }
        }
        true
    }

    fn take_link_control(&mut self, link_control: LinkControl) {
        if link_control.encrypted && self.privacy.is_none() && self.decryptor.is_none() {
            self.decryptor = crate::crypto::VoiceDecryptor::for_dmr_basic(
                link_control.feature_id,
                link_control.target_id,
            );
        }
        self.link_control = Some(link_control);
    }

    /// True only when both sides named a timeslot and they agree.
    /// Unknown CACH must not look like "our" slot — that is how other-slot
    /// idle was ending live calls after a single burst.
    fn same_known_slot(&self, slot: Option<u8>) -> bool {
        match (self.follow_slot, slot) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }

    fn cc_ok(&self, burst: &Burst) -> Option<bool> {
        match (self.spec.color_code, burst.color_code()) {
            (None, _) => Some(true),
            (Some(_), None) => None,
            (Some(want), Some(got)) => Some(want == got),
        }
    }

    fn decode_voice(&mut self, burst: &Burst) -> Vec<f32> {
        let frames = voice::extract(&burst.payload());
        let mut pcm = Vec::new();
        for frame in &frames {
            self.voice_error_rate =
                0.95 * self.voice_error_rate + 0.001_064 * f32::from(frame.errors);
            let reliable = frame.valid && frame.errors <= 4 && self.voice_error_rate <= 0.096;
            if reliable {
                let mut data = frame.data;
                if let Some(decryptor) = &mut self.decryptor {
                    decryptor.apply_ambe49(&mut data);
                }
                if let Ok(samples) = self.vocoder.decode(&data) {
                    pcm.extend(samples);
                    continue;
                }
            }
            pcm.extend([0i16; rmbe::SAMPLES_PER_FRAME]);
        }
        let mut audio = self.resampler.process(&pcm);
        self.leveler.process(&mut audio);
        audio
    }

    fn take_voice(&mut self, burst: &Burst, pcm: Vec<f32>, snr_db: f32) -> Option<CallEvent> {
        match self.cc_ok(burst) {
            Some(false) => {
                self.pending.clear();
                if self.in_call {
                    return self.end_call();
                }
                None
            }
            None if !self.in_call => {
                // Colour code not yet known; hold the audio as digital pre-roll.
                self.pending.extend(pcm);
                self.follow_slot = burst.slot.or(self.follow_slot);
                None
            }
            _ => {
                if !self.in_call {
                    self.in_call = true;
                    self.call_samples = 0;
                    self.peak_snr_db = snr_db.max(self.rx.quality_db());
                    self.hang_s = 0.0;
                    self.follow_slot = burst.slot.or(self.follow_slot);
                    self.tone = burst.color_code().map(|cc| ToneCode::Dmr {
                        color_code: cc,
                        slot: burst.slot,
                    });
                    self.out.extend(self.pending.drain(..));
                    self.out.extend(pcm);
                    self.call_samples += self.out.len();
                    Some(CallEvent::Started)
                } else {
                    if self.tone.is_none() {
                        self.tone = burst.color_code().map(|cc| ToneCode::Dmr {
                            color_code: cc,
                            slot: burst.slot,
                        });
                    }
                    self.out.extend(pcm);
                    self.call_samples += self.out.len();
                    None
                }
            }
        }
    }

    fn end_call(&mut self) -> Option<CallEvent> {
        if !self.in_call {
            self.pending.clear();
            self.follow_slot = None;
            return None;
        }
        let tone = self.tone.take();
        let color_code = match tone.as_ref() {
            Some(ToneCode::Dmr { color_code, .. }) => Some(*color_code),
            _ => self.spec.color_code,
        };
        let slot = match tone.as_ref() {
            Some(ToneCode::Dmr { slot, .. }) => *slot,
            _ => self.follow_slot.or(self.spec.slot),
        };
        self.in_call = false;
        self.follow_slot = None;
        self.pending.clear();
        self.hang_s = 0.0;
        // Captured before the reset below: it is the summary's record of how
        // heavily the voice was being corrected, which is what tells a
        // listener "scrambled" from "clean".
        let bit_error_pct = Some((self.voice_error_rate * 100.0).clamp(0.0, 100.0) as u8);
        self.voice_error_rate = 0.0;
        let encrypted =
            self.link_control.as_ref().is_some_and(|lc| lc.encrypted) || self.privacy.is_some();
        let summary = CallSummary {
            audio_samples: self.call_samples,
            peak_snr_db: self.peak_snr_db,
            tone,
            mdc: None,
            freq_error_hz: if self.offset_n > 0 {
                (self.offset_sum / self.offset_n as f64) as f32
            } else {
                0.0
            },
            voiced_fraction: 1.0,
            digital: Some(DigitalCallTelemetry {
                protocol: self
                    .link_control
                    .as_ref()
                    .and_then(|lc| {
                        (lc.feature_id == 0x10 && lc.capacity_plus_rest_lsn.is_some())
                            .then_some("DMR Capacity Plus")
                    })
                    .unwrap_or("DMR")
                    .into(),
                color_code,
                slot,
                source_id: self.link_control.as_ref().map(|lc| lc.source_id),
                target_id: self.link_control.as_ref().map(|lc| lc.target_id),
                group: self.link_control.as_ref().map(|lc| lc.group),
                manufacturer: self
                    .link_control
                    .as_ref()
                    .map(|lc| lc.manufacturer.clone())
                    .or_else(|| self.privacy.as_ref().map(|pi| pi.manufacturer.clone())),
                service_options: self.link_control.as_ref().map(|lc| lc.service_options),
                emergency: self.link_control.as_ref().is_some_and(|lc| lc.emergency),
                encrypted,
                decrypted: encrypted && self.decryptor.is_some(),
                algorithm_id: self.privacy.as_ref().map(|pi| pi.algorithm_id),
                key_id: self.privacy.as_ref().map(|pi| u16::from(pi.key_id)),
                talker_alias: self
                    .link_control
                    .as_ref()
                    .and_then(|lc| lc.talker_alias.clone()),
                bit_error_pct,
            }),
        };
        self.link_control = None;
        self.privacy = None;
        self.decryptor = None;
        for assembler in &mut self.embedded_lc {
            assembler.clear();
        }
        self.peak_snr_db = f32::NEG_INFINITY;
        self.call_samples = 0;
        Some(CallEvent::Ended(summary))
    }
}

fn decode_hex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .chunks_exact(2)
        .filter_map(|pair| {
            let hi = (pair[0] as char).to_digit(16)?;
            let lo = (pair[1] as char).to_digit(16)?;
            Some(((hi << 4) | lo) as u8)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dmr::framer::{
        BurstKind, DT_IDLE, encode_cach, encode_data_burst, encode_voice_burst,
        encode_voice_emb_burst, modulate,
    };
    use crate::dmr::sync::SyncKind;
    use crate::dmr::voice::{self, VoiceFrame};
    use scannerd_dsp::DecodeChain;

    fn spec() -> DmrSpec {
        DmrSpec {
            name: "test".into(),
            freq_hz: 0.0,
            color_code: None,
            slot: None,
        }
    }

    fn voice_frame(seed: u8) -> VoiceFrame {
        VoiceFrame {
            data: [seed, 0, 0, 0, 0, 0, 0x80],
            errors: 0,
            valid: true,
        }
    }

    fn stream(frames: usize) -> Vec<Complex32> {
        let payload = voice::assemble(&[voice_frame(0x10), voice_frame(0x20), voice_frame(0x30)]);
        let mut dibits = Vec::new();
        for i in 0..frames {
            if i % 2 == 0 {
                dibits.extend(encode_cach(1));
                dibits.extend(encode_voice_burst(&payload, SyncKind::BsVoice));
            } else {
                dibits.extend(encode_cach(2));
                dibits.extend(encode_data_burst(1, DT_IDLE, SyncKind::BsData));
            }
        }
        modulate(&dibits, CHANNEL_RATE)
    }

    #[test]
    fn a_clean_signal_opens_and_closes_a_call() {
        let mut rx = DmrChannelReceiver::new(spec(), CHANNEL_RATE, 0.0);
        let iq = stream(8);
        let mut started = false;
        let ev = rx.process(&iq, 20.0);
        started |= matches!(ev, Some(CallEvent::Started));
        assert!(started || rx.in_call(), "call should open on voice");
        assert!(!rx.audio().is_empty() || rx.in_call());

        // Silence long enough for hang + lock loss.
        let quiet = vec![Complex32::new(0.0, 0.0); 48_000];
        let mut ended = false;
        for _ in 0..4 {
            if matches!(rx.process(&quiet, 0.0), Some(CallEvent::Ended(_))) {
                ended = true;
                break;
            }
        }
        assert!(ended, "call should close after the signal drops");
        assert!(!rx.in_call());
    }

    #[test]
    fn idle_on_the_other_slot_does_not_end_the_call() {
        let mut rx = DmrChannelReceiver::new(spec(), CHANNEL_RATE, 0.0);
        assert!(rx.process(&stream(8), 20.0).is_some() || rx.in_call());
        // One TDMA frame of the other timeslot sitting idle, as a repeater
        // does every 30 ms while our slot is talking.
        let mut idle = encode_cach(2).to_vec();
        idle.extend(encode_data_burst(1, DT_IDLE, SyncKind::BsData));
        let ev = rx.process(&modulate(&idle, CHANNEL_RATE), 20.0);
        assert!(
            ev.is_none(),
            "other-slot idle must not close the call: {ev:?}"
        );
        assert!(rx.in_call());
    }

    #[test]
    fn a_short_gap_between_bursts_does_not_end_the_call() {
        let mut rx = DmrChannelReceiver::new(spec(), CHANNEL_RATE, 0.0);
        assert!(rx.process(&stream(8), 20.0).is_some() || rx.in_call());
        // ~80 ms of silence: several analog-scanner IQ blocks, well under hang.
        let gap = vec![Complex32::new(0.0, 0.0); 4_000];
        assert!(rx.process(&gap, 0.0).is_none());
        assert!(rx.in_call(), "0.6 s hang must outlast a 60 ms TDMA gap");
    }

    #[test]
    fn a_colour_code_filter_rejects_the_wrong_cc() {
        let mut spec = spec();
        spec.color_code = Some(4);
        let mut rx = DmrChannelReceiver::new(spec, CHANNEL_RATE, 0.0);
        // The helper stream's voice bursts have no EMB, so CC never confirms.
        let ev = rx.process(&stream(6), 20.0);
        assert!(ev.is_none());
        assert!(!rx.in_call());
    }

    /// Two voice superframes on a repeater, with EMB colour code 10, at the
    /// analog scanner's radio rate so the IQ goes through DecodeChain's
    /// integer decimation (÷43 → 47627.9 Hz).
    fn analog_repeater_iq() -> (f64, Vec<Complex32>) {
        let fs = 2_048_000.0;
        let payload = voice::assemble(&[voice_frame(0x10), voice_frame(0x20), voice_frame(0x30)]);
        let mut dibits = Vec::new();
        for _ in 0..2 {
            dibits.extend(encode_cach(1));
            dibits.extend(encode_data_burst(10, DT_IDLE, SyncKind::BsData));
            dibits.extend(encode_cach(2));
            dibits.extend(encode_data_burst(10, DT_IDLE, SyncKind::BsData));
        }
        for _ in 0..3 {
            dibits.extend(encode_cach(1));
            dibits.extend(encode_voice_burst(&payload, SyncKind::BsVoice));
            dibits.extend(encode_cach(2));
            dibits.extend(encode_data_burst(10, DT_IDLE, SyncKind::BsData));
            for _ in 0..5 {
                dibits.extend(encode_cach(1));
                dibits.extend(encode_voice_emb_burst(&payload, 10));
                dibits.extend(encode_cach(2));
                dibits.extend(encode_data_burst(10, DT_IDLE, SyncKind::BsData));
            }
        }
        (fs, modulate(&dibits, fs))
    }

    #[test]
    fn analog_rate_framer_sees_emb_through_the_decode_chain() {
        let (fs, iq) = analog_repeater_iq();
        let mut chain = DecodeChain::new(fs, DMR_BANDWIDTH_HZ, CHANNEL_RATE);
        let mut bb = Vec::new();
        chain.process(&iq, &mut bb);
        let mut rx = crate::dmr::DmrReceiver::new(chain.fs_out());
        let bursts = rx.process(&bb);
        let voice: Vec<_> = bursts.iter().filter(|b| b.is_voice()).collect();
        let emb = voice
            .iter()
            .filter(|b| matches!(b.kind, BurstKind::Voice { emb: Some(_), .. }))
            .count();
        assert!(
            voice.len() >= 10,
            "DecodeChain must not drop the superframe: {} voice / {} total",
            voice.len(),
            bursts.len()
        );
        assert!(emb >= 8, "EMB bursts lost through DecodeChain: {emb}");
    }

    #[test]
    fn analog_rate_call_lasts_a_whole_superframe() {
        let (fs, iq) = analog_repeater_iq();
        let mut spec = spec();
        spec.color_code = Some(10);
        let mut rx = DmrChannelReceiver::new(spec, fs, 0.0);
        // Scanner-sized chunks, not one giant block: AFC and the hang
        // timer both run per block in production.
        let chunk = 16_384;
        let mut started = false;
        let mut audio = 0usize;
        for block in iq.chunks(chunk) {
            let ev = rx.process(block, 20.0);
            started |= matches!(ev, Some(CallEvent::Started));
            audio += rx.audio().len();
        }
        assert!(
            started || rx.in_call(),
            "CC10 should open the call once EMB confirms"
        );
        let duration = audio as f32 / AUDIO_RATE as f32;
        assert!(
            duration > 0.40,
            "timing slip used to chop every DMR hit to 0.06–0.12 s; got {duration:.3} s"
        );

        let Some(CallEvent::Ended(summary)) = rx.end_call() else {
            panic!("call should close with structured telemetry");
        };
        assert_eq!(
            summary.tone,
            Some(ToneCode::Dmr {
                color_code: 10,
                slot: Some(1)
            })
        );
        let digital = summary.digital.expect("DMR telemetry");
        assert_eq!(digital.color_code, Some(10));
        assert_eq!(digital.slot, Some(1));
    }
}
