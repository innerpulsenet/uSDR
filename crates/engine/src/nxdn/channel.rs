//! Conventional NXDN48/NXDN96 voice channel.

use super::{CHANNEL_RATE, NXDN_BANDWIDTH_HZ, NxdnReceiver, Rate};
use super::{ControlMessage, decode_control};
use crate::afc::Afc;
use crate::channel::{AUDIO_RATE, CallEvent, CallSummary, DigitalCallTelemetry};
use crate::dmr::Resampler8k;
use crate::leveler::Leveler;
use num_complex::Complex32;
use scannerd_dsp::DecodeChain;

const HANG_S: f32 = 0.72;

#[derive(Clone, Debug, PartialEq)]
pub struct NxdnSpec {
    pub name: String,
    pub freq_hz: f64,
    /// `None` runs NXDN48 and NXDN96 acquisition concurrently.
    pub rate: Option<Rate>,
    /// Optional Radio Access Number (0–63). Applied once SACCH is recovered.
    pub ran: Option<u8>,
}

/// A voice-call assignment heard on a control channel: the site telling a
/// radio which carrier to move to. Surfaced so the application layer can log
/// or follow traffic, as dsd-fme does for Type-C.
#[derive(Clone, Debug, PartialEq)]
pub struct NxdnGrant {
    pub talkgroup: u32,
    pub group: bool,
    pub source: u32,
    /// Carrier number from the assignment. NXDN does not announce a band
    /// plan in-band, so converting this to Hz is the operator's job — but
    /// consecutive grants on the same number are directly comparable.
    pub channel_number: u16,
}

pub struct NxdnChannelReceiver {
    pub spec: NxdnSpec,
    chain: DecodeChain,
    afc: Afc,
    rx48: NxdnReceiver,
    rx96: NxdnReceiver,
    vocoder: rmbe::Decoder,
    resampler: Resampler8k,
    leveler: Leveler,
    baseband: Vec<Complex32>,
    out: Vec<f32>,
    in_call: bool,
    call_samples: usize,
    peak_snr_db: f32,
    hang_s: f32,
    locked_rate: Option<Rate>,
    system: Option<String>,
    control: Option<ControlMessage>,
    offset_sum: f64,
    offset_n: usize,
    /// Call assignments since the last drain.
    grants: std::collections::VecDeque<NxdnGrant>,
}

impl NxdnChannelReceiver {
    pub fn new(spec: NxdnSpec, fs_in: f64, span_center_hz: f64) -> Self {
        let chain = DecodeChain::new(fs_in, NXDN_BANDWIDTH_HZ, CHANNEL_RATE);
        let mut out = Self::build(spec, chain);
        out.retune(span_center_hz);
        out
    }

    /// Build for input that is already the extracted channel: baseband
    /// centred on `spec.freq_hz` at `channel_fs` — a narrowband chain's
    /// output the receiver rides instead of re-extracting its own channel
    /// from the whole span, the same arrangement P25/DMR use in the server.
    /// The chain collapses to decimation 1, and its rate grid makes `fs_out`
    /// exactly what the span-rate build would have produced
    /// (`span / round(span / CHANNEL_RATE)`), so every downstream
    /// symbol-timing constant is unchanged. The AFC base is zero: the
    /// carrier arrives at DC and only residual ±2 kHz tracking remains.
    pub fn new_on_channel(spec: NxdnSpec, channel_fs: f64) -> Self {
        Self::build(spec, DecodeChain::new(channel_fs, NXDN_BANDWIDTH_HZ, CHANNEL_RATE))
    }

    fn build(spec: NxdnSpec, chain: DecodeChain) -> Self {
        // Build the symbol-timing receivers at the chain's ACTUAL output rate,
        // not the nominal CHANNEL_RATE. A span-rate build lands on
        // `span / round(span / 48 kHz)` (47 627.9 Hz at a 2.048 MS/s span),
        // and `new_on_channel` inherits that grid from the inspect chain. The
        // discriminator's hz-per-radian must match the rate the samples really
        // arrive at, or every level is scaled by the clock error — the same
        // reason P25 builds its front end at `chain.fs_out()`.
        let fs_chan = chain.fs_out();
        Self {
            chain,
            afc: Afc::new(0.0, 2_000.0),
            rx48: NxdnReceiver::new(Rate::Nxdn48, fs_chan),
            rx96: NxdnReceiver::new(Rate::Nxdn96, fs_chan),
            vocoder: rmbe::Decoder::new(),
            resampler: Resampler8k::new(),
            leveler: Leveler::new(AUDIO_RATE),
            baseband: Vec::new(),
            out: Vec::new(),
            in_call: false,
            call_samples: 0,
            peak_snr_db: f32::NEG_INFINITY,
            hang_s: 0.0,
            locked_rate: None,
            system: None,
            control: None,
            offset_sum: 0.0,
            offset_n: 0,
            grants: std::collections::VecDeque::new(),
            spec,
        }
    }

    pub fn retune(&mut self, span_center_hz: f64) {
        self.afc.set_base(self.spec.freq_hz - span_center_hz);
        self.chain.set_offset(self.afc.mix_hz());
    }

    pub fn reset(&mut self) {
        self.rx48.reset();
        self.rx96.reset();
        self.vocoder = rmbe::Decoder::new();
        self.resampler = Resampler8k::new();
        self.leveler.reset();
        self.out.clear();
        self.in_call = false;
        self.call_samples = 0;
        self.peak_snr_db = f32::NEG_INFINITY;
        self.hang_s = 0.0;
        self.locked_rate = None;
        self.system = None;
        self.control = None;
        self.offset_sum = 0.0;
        self.offset_n = 0;
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
        self.locked_rate.is_some()
    }

    /// Which symbol rate locked, for display ("NXDN48"/"NXDN96"); `None`
    /// while still searching.
    pub fn rate_label(&self) -> Option<&'static str> {
        self.locked_rate.map(|rate| rate.label())
    }

    pub fn process(&mut self, iq: &[Complex32], snr_db: f32) -> Option<CallEvent> {
        self.out.clear();
        self.chain.process(iq, &mut self.baseband);
        if self.baseband.is_empty() {
            return None;
        }
        let mut frames = Vec::new();
        if self.spec.rate != Some(Rate::Nxdn96) {
            frames.extend(self.rx48.process(&self.baseband));
        }
        if self.spec.rate != Some(Rate::Nxdn48) {
            frames.extend(self.rx96.process(&self.baseband));
        }
        frames.sort_by(|a, b| b.correlation.total_cmp(&a.correlation));
        let mut event = None;
        let mut heard_voice = false;
        for frame in frames {
            if self.locked_rate.is_some_and(|rate| rate != frame.rate) && self.in_call {
                continue;
            }
            let control = decode_control(&frame);
            if let Some(want) = self.spec.ran
                && control.iter().any(|m| m.ran.is_some_and(|ran| ran != want))
            {
                continue;
            }
            if let Some(message) = control.iter().find(|m| {
                m.source_id.is_some()
                    || m.target_id.is_some()
                    || m.cipher_type.is_some()
                    || m.ran.is_some()
            }) {
                self.control = Some(message.clone());
            }
            self.observe_grants(&control);
            self.locked_rate = Some(frame.rate);
            self.system = Some(frame.system.label().into());
            self.afc.observe(frame.offset_hz, true);
            self.chain.set_offset(self.afc.mix_hz());
            self.offset_sum += f64::from(self.afc.correction_hz() + frame.offset_hz);
            self.offset_n += 1;
            if !frame.has_voice() {
                continue;
            }
            heard_voice = true;
            let encrypted = self
                .control
                .as_ref()
                .and_then(|m| m.cipher_type)
                .unwrap_or(0)
                != 0;
            let mut pcm = Vec::new();
            for voice in frame.voice_blocks {
                if !encrypted && voice.valid && voice.errors <= 4 {
                    if let Ok(samples) = self.vocoder.decode(&voice.data) {
                        pcm.extend(samples);
                        continue;
                    }
                }
                pcm.extend([0i16; rmbe::SAMPLES_PER_FRAME]);
            }
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
            self.hang_s = 0.0;
            self.peak_snr_db = self.peak_snr_db.max(snr_db);
        }
        if self.in_call && !heard_voice {
            self.hang_s += self.baseband.len() as f32 / self.chain.fs_out() as f32;
            if self.hang_s >= HANG_S {
                event = self.end_call().or(event);
            }
        }
        event
    }

    /// End an in-progress call because the receiver is being taken away --
    /// a span hop, or a trunking grant borrowing the radio.
    pub fn finish(&mut self) -> Option<CallEvent> {
        self.end_call()
    }

    /// A call assignment on a control channel is a trunking grant: the site
    /// is moving that radio (and everyone listening) to a named carrier.
    /// Queued for the application layer.
    fn observe_grants(&mut self, control: &[ControlMessage]) {
        for message in control {
            let (Some(source), Some(target), Some(channel)) =
                (message.source_id, message.target_id, message.channel_number)
            else {
                continue;
            };
            if self.grants.len() >= 8 {
                self.grants.pop_front();
            }
            self.grants.push_back(NxdnGrant {
                talkgroup: u32::from(target),
                group: message.group.unwrap_or(true),
                source: u32::from(source),
                channel_number: channel,
            });
        }
    }

    /// Call assignments observed since the last call to this method.
    pub fn pending_grants(&mut self) -> Vec<NxdnGrant> {
        self.grants.drain(..).collect()
    }

    fn end_call(&mut self) -> Option<CallEvent> {
        if !self.in_call {
            return None;
        }
        self.in_call = false;
        self.hang_s = 0.0;
        let rate = self.locked_rate;
        let control = self.control.take();
        let protocol = match (rate, self.system.as_deref()) {
            (Some(rate), Some(system)) => format!("{} · {system}", rate.label()),
            (Some(rate), None) => rate.label().into(),
            _ => "NXDN".into(),
        };
        let summary = CallSummary {
            audio_samples: self.call_samples,
            peak_snr_db: self.peak_snr_db,
            tone: None,
            mdc: None,
            freq_error_hz: if self.offset_n > 0 {
                (self.offset_sum / self.offset_n as f64) as f32
            } else {
                0.0
            },
            voiced_fraction: 1.0,
            digital: Some(DigitalCallTelemetry {
                protocol,
                color_code: None,
                slot: None,
                source_id: control.as_ref().and_then(|m| m.source_id).map(u32::from),
                target_id: control.as_ref().and_then(|m| m.target_id).map(u32::from),
                group: control.as_ref().and_then(|m| m.group),
                manufacturer: Some("NXDN Forum".into()),
                service_options: control.as_ref().and_then(|m| m.service_options),
                emergency: control.as_ref().is_some_and(|m| m.emergency),
                encrypted: control.as_ref().and_then(|m| m.cipher_type).unwrap_or(0) != 0,
                decrypted: false,
                algorithm_id: control.as_ref().and_then(|m| m.cipher_type),
                key_id: control.as_ref().and_then(|m| m.key_id).map(u16::from),
                talker_alias: None,
                bit_error_pct: None,
            }),
        };
        self.call_samples = 0;
        self.peak_snr_db = f32::NEG_INFINITY;
        self.offset_sum = 0.0;
        self.offset_n = 0;
        Some(CallEvent::Ended(summary))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The server rides `new_on_channel`: the receiver is fed baseband
    /// already centred on the channel, and its internal chain collapses to
    /// decimation 1. That build path must still lock onto valid frames — the
    /// same acquisition the span-rate build does. The rate-grid itself
    /// (span-rate vs on-channel `fs_out` equality) is pinned separately by
    /// the server's `channelized_receivers_keep_the_span_rate_grid`; here the
    /// chain and the fixture are both at CHANNEL_RATE so the test isolates
    /// the wiring, not a clock offset.
    #[test]
    fn on_channel_build_locks_frames_from_channel_baseband() {
        let lich = 0x57u8;
        let iq = super::super::tests_modulated_frames(lich);
        let mut rx = NxdnChannelReceiver::new_on_channel(
            NxdnSpec {
                name: "test".into(),
                freq_hz: 450_100_000.0,
                rate: Some(Rate::Nxdn96),
                ran: None,
            },
            CHANNEL_RATE,
        );
        // The on-channel chain still runs its anti-alias FIR (2047 taps),
        // which swallows ~2046 samples of warm-up before it emits; three
        // frames (5760 samples) would leave under two usable frames and
        // acquisition needs consecutive valid ones. Feed the burst twice.
        let stream = [iq.as_slice(), iq.as_slice()].concat();
        for block in stream.chunks(384) {
            rx.process(block, 20.0);
        }
        assert!(rx.locked(), "on-channel build never locked a valid frame");
    }

    /// A Voice Call Assignment heard on a control channel queues a grant the
    /// application can act on — this is the surface trunk-follow uses.
    #[test]
    fn call_assignments_queue_as_grants() {
        let mut rx = NxdnChannelReceiver::new(
            NxdnSpec {
                name: "t".into(),
                freq_hz: 450_100_000.0,
                rate: Some(crate::nxdn::Rate::Nxdn48),
                ran: None,
            },
            960_000.0,
            450_100_000.0,
        );
        let assignment = ControlMessage {
            channel: "FACCH1-A".into(),
            message_type: 0x04,
            message_name: "Voice Call Assignment".into(),
            ran: Some(7),
            source_id: Some(123),
            target_id: Some(4567),
            group: Some(true),
            channel_number: Some(310),
            location_id: None,
            site_code: None,
            service_options: Some(0x80),
            emergency: true,
            cipher_type: None,
            key_id: None,
            valid: true,
            raw: String::new(),
        };
        rx.observe_grants(&[assignment]);
        let pending = rx.pending_grants();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].talkgroup, 4567);
        assert_eq!(pending[0].source, 123);
        assert_eq!(pending[0].channel_number, 310);
        assert!(pending[0].group);
        // Drained: nothing twice.
        assert!(rx.pending_grants().is_empty());
    }
}
