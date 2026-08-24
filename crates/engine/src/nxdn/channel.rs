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
}

impl NxdnChannelReceiver {
    pub fn new(spec: NxdnSpec, fs_in: f64, span_center_hz: f64) -> Self {
        let chain = DecodeChain::new(fs_in, NXDN_BANDWIDTH_HZ, CHANNEL_RATE);
        let mut out = Self {
            chain,
            afc: Afc::new(0.0, 2_000.0),
            rx48: NxdnReceiver::new(Rate::Nxdn48, CHANNEL_RATE),
            rx96: NxdnReceiver::new(Rate::Nxdn96, CHANNEL_RATE),
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
            spec,
        };
        out.retune(span_center_hz);
        out
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
            if let Some(message) = control.into_iter().find(|m| {
                m.source_id.is_some()
                    || m.target_id.is_some()
                    || m.cipher_type.is_some()
                    || m.ran.is_some()
            }) {
                self.control = Some(message);
            }
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
            }),
        };
        self.call_samples = 0;
        self.peak_snr_db = f32::NEG_INFINITY;
        self.offset_sum = 0.0;
        self.offset_n = 0;
        Some(CallEvent::Ended(summary))
    }
}
