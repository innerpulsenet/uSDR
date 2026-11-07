//! The second receiver: one more listening channel inside the same span.
//!
//! It is a listener only — NFM, AM, sideband or broadcast FM through the
//! same channel chain the main receiver uses, with no classifier and no
//! decoders behind it. It rides the span: when the span moves it rebuilds
//! for the new centre, and if its frequency is no longer inside the span it
//! stops and says why, rather than playing whatever the chain now points at.

use crate::sdr::{SdrMode, WFM_DEEMPHASIS_TAU, WFM_DEVIATION_HZ, wfm_audio_filter};
use num_complex::Complex32;
use scannerd_dsp::{DecodeChain, OnePole};
use scannerd_engine::am::AmDemod;
use scannerd_engine::biquad::Cascade;
use scannerd_engine::leveler::Leveler;
use scannerd_engine::nbfm::{Demodulated, NbfmDemod};
use scannerd_engine::noisegate::NoiseGate;
use scannerd_engine::ssb::{Sideband, SsbDemod};
use scannerd_engine::stereo::{WFM_AUDIO_SCALE, WFM_DC_TAU_S};
use serde::Serialize;

/// The audio rate the analogue paths aim for.
const AUDIO_RATE: f64 = 48_000.0;

/// The modes a second receiver can listen in.
pub fn supports(mode: SdrMode) -> bool {
    matches!(mode, SdrMode::Nfm | SdrMode::Am | SdrMode::Usb | SdrMode::Lsb | SdrMode::Wfm)
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct SubRxStatus {
    pub freq_hz: f64,
    pub mode: SdrMode,
    /// Passband width and its centre's offset from the carrier, as the main
    /// receiver reports its own.
    pub bandwidth_hz: f64,
    pub passband_offset_hz: f64,
    /// Its mode's filter shape.
    pub filter_shape: crate::sdr::FilterShape,
    /// Why it is silent, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

enum Demod {
    /// Receiver A's NFM listening chain: de-emphasis and the 300 Hz-3 kHz
    /// voice band (which keeps CTCSS out), the noise gate, the leveler.
    /// The raw discriminator this used to play had none of them: static
    /// whenever the channel was idle, the tone under every call, and the
    /// treble 6 dB/octave too loud.
    Fm { fm: NbfmDemod, out: Demodulated, gate: NoiseGate, leveler: Leveler },
    Am { am: AmDemod },
    Ssb { ssb: SsbDemod },
    Wfm { fm: NbfmDemod, disc: Vec<f32>, deemph: OnePole, lp: Cascade, dc: OnePole, decim: usize, phase: usize },
}

pub struct SubRx {
    pub freq_hz: f64,
    pub mode: SdrMode,
    /// (span rate, span centre, LO offset) the chain was built for.
    built_for: Option<(f64, f64, f64)>,
    chain: Option<DecodeChain>,
    demod: Option<Demod>,
    iq: Vec<Complex32>,
    audio: Vec<f32>,
    pub note: Option<String>,
}

impl SubRx {
    pub fn new(freq_hz: f64, mode: SdrMode) -> Self {
        Self { freq_hz, mode, built_for: None, chain: None, demod: None, iq: Vec::new(), audio: Vec::new(), note: None }
    }

    /// Rebuild before the next block (its mode's filter changed).
    pub fn invalidate(&mut self) {
        self.built_for = None;
    }

    pub fn status(&self) -> SubRxStatus {
        let (lo, hi) = self.mode.filter_edges();
        SubRxStatus {
            freq_hz: self.freq_hz,
            mode: self.mode,
            bandwidth_hz: f64::from(hi - lo),
            passband_offset_hz: f64::from((lo + hi) / 2.0),
            filter_shape: self.mode.filter_shape(),
            note: self.note.clone(),
        }
    }

    fn build(&mut self, rate: f64, centre: f64, lo_off: f64) {
        let target = if self.mode == SdrMode::Wfm { crate::sdr::WFM_IF_RATE } else { AUDIO_RATE };
        let mut chain = DecodeChain::new(rate, self.mode.chain_bandwidth(), target);
        chain.set_offset(self.freq_hz - centre - lo_off);
        let fs = chain.fs_out();
        self.demod = Some(match self.mode {
            SdrMode::Am => {
                // The audio band follows the filter, as on receiver A.
                let (lo, hi) = self.mode.filter_edges();
                Demod::Am { am: AmDemod::with_audio_lowpass(fs, ((hi - lo) / 2.0).clamp(2_500.0, 10_000.0)) }
            }
            SdrMode::Usb | SdrMode::Lsb => {
                let (lo, hi) = self.mode.filter_edges();
                let side = if self.mode == SdrMode::Usb { Sideband::Upper } else { Sideband::Lower };
                Demod::Ssb { ssb: SsbDemod::with_shape(fs, side, lo.abs().min(hi.abs()), lo.abs().max(hi.abs()), self.mode.filter_shape()) }
            }
            SdrMode::Wfm => Demod::Wfm {
                fm: NbfmDemod::with_deviation(fs, WFM_DEVIATION_HZ),
                disc: Vec::new(),
                deemph: OnePole::new(WFM_DEEMPHASIS_TAU * fs as f32),
                // Band-limited before it is decimated, as on receiver A: an
                // average alone folds the pilot and the stereo subcarrier
                // back into the audio as whistle and hiss.
                lp: wfm_audio_filter(fs),
                // Receiver A's mono, exactly: AC-coupled, at its scale.
                dc: OnePole::new(WFM_DC_TAU_S * fs as f32),
                decim: (fs / AUDIO_RATE).round().max(1.0) as usize,
                phase: 0,
            },
            _ => Demod::Fm {
                // At the deviation the gate's levels were measured with.
                fm: NbfmDemod::new(fs),
                out: Demodulated::default(),
                gate: NoiseGate::new(fs),
                leveler: Leveler::new(fs),
            },
        });
        self.chain = Some(chain);
        self.built_for = Some((rate, centre, lo_off));
    }

    /// Run one block of the span through this receiver. Returns its audio
    /// and the rate it is at, or `None` while it has nothing to say.
    pub fn process(&mut self, clean: &[Complex32], rate: f64, centre: f64, lo_off: f64) -> Option<(u32, Vec<i16>)> {
        if clean.is_empty() || rate <= 0.0 {
            return None;
        }
        // Inside the span, clear of its edges by the channel's half width.
        let half = f64::from(self.mode.chain_bandwidth()) / 2.0;
        let lo = centre + lo_off - rate / 2.0 + half;
        let hi = centre + lo_off + rate / 2.0 - half;
        if self.freq_hz < lo || self.freq_hz > hi {
            self.note = Some("outside the span".into());
            self.chain = None;
            self.built_for = None;
            return None;
        }
        self.note = None;
        if self.built_for != Some((rate, centre, lo_off)) {
            self.build(rate, centre, lo_off);
        }
        let chain = self.chain.as_mut()?;
        let fs = chain.fs_out();
        self.iq.clear();
        chain.process(clean, &mut self.iq);
        self.audio.clear();
        let out_rate = match self.demod.as_mut()? {
            Demod::Am { am } => {
                am.process(&self.iq, &mut self.audio);
                fs
            }
            Demod::Ssb { ssb } => {
                ssb.process(&self.iq, &mut self.audio);
                fs
            }
            Demod::Fm { fm, out, gate, leveler } => {
                fm.process(&self.iq, out);
                gate.process(&mut out.voice, &out.noise);
                leveler.process(&mut out.voice);
                self.audio.extend(out.voice.iter().map(|v| v.clamp(-1.0, 1.0)));
                fs
            }
            Demod::Wfm { fm, disc, deemph, lp, dc, decim, phase } => {
                fm.discriminator_hz(&self.iq, disc);
                for &hz in disc.iter() {
                    let v = lp.process(deemph.process(hz / WFM_DEVIATION_HZ));
                    let v = v - dc.process(v);
                    *phase += 1;
                    if *phase >= *decim {
                        self.audio.push((v * WFM_AUDIO_SCALE).clamp(-1.0, 1.0));
                        *phase = 0;
                    }
                }
                fs / *decim as f64
            }
        };
        if self.audio.is_empty() {
            return None;
        }
        Some((out_rate.round() as u32, self.audio.iter().map(|&x| (x * 28_000.0) as i16).collect()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A carrier 100 kHz above the centre of a 1 MS/s span, FM-modulated by
    /// a 1 kHz tone at 2.5 kHz deviation.
    fn span_with_fm_tone(n: usize) -> Vec<Complex32> {
        span_with_fm(n, &[(1e3, 2.5e3)])
    }

    /// The same carrier, modulated by `tones` (Hz, deviation Hz).
    fn span_with_fm(n: usize, tones: &[(f64, f64)]) -> Vec<Complex32> {
        let fs = 1e6;
        let mut phase = 0.0f64;
        (0..n)
            .map(|i| {
                let t = i as f64 / fs;
                let f = 100e3 + tones.iter().map(|&(hz, dev)| dev * (2.0 * std::f64::consts::PI * hz * t).sin()).sum::<f64>();
                phase += 2.0 * std::f64::consts::PI * f / fs;
                Complex32::new(phase.cos() as f32 * 0.3, phase.sin() as f32 * 0.3)
            })
            .collect()
    }

    /// Receiver B's audio for `iq` in NFM, and its rate.
    fn listen(iq: &[Complex32]) -> (u32, Vec<f32>) {
        let mut rx = SubRx::new(100.1e6, SdrMode::Nfm);
        let mut audio = Vec::new();
        let mut rate = 0;
        for block in iq.chunks(20_000) {
            if let Some((r, a)) = rx.process(block, 1e6, 100e6, 0.0) {
                rate = r;
                audio.extend(a.iter().map(|&v| v as f32 / 28_000.0));
            }
        }
        (rate, audio)
    }

    fn amp_at(x: &[f32], hz: f64, fs: f64) -> f32 {
        let w = 2.0 * std::f64::consts::PI * hz / fs;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, &s) in x.iter().enumerate() {
            re += s as f64 * (w * i as f64).cos();
            im += s as f64 * (w * i as f64).sin();
        }
        (2.0 * (re * re + im * im).sqrt() / x.len() as f64) as f32
    }

    /// A CTCSS tone under the voice stays out of the audio, as it does on
    /// receiver A. The raw discriminator played it at its deviation: a
    /// 100 Hz tone at a fifth of the voice's level.
    #[test]
    fn nfm_keeps_the_subaudible_tone_out() {
        let (rate, audio) = listen(&span_with_fm(400_000, &[(1e3, 2.5e3), (100.0, 500.0)]));
        let tail = &audio[audio.len() / 2..];
        let voice = amp_at(tail, 1e3, f64::from(rate));
        let tone = amp_at(tail, 100.0, f64::from(rate));
        let db = 20.0 * (tone / voice).log10();
        eprintln!("CTCSS {db:.1} dB under the voice");
        assert!(db < -40.0, "CTCSS only {db:.1} dB under the voice");
    }

    /// An idle channel (noise, no carrier) is gated, not played as static.
    #[test]
    fn nfm_idle_channel_is_quiet() {
        let mut s = 0x1234_5678u32;
        let mut uniform = move || {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            s as f32 / u32::MAX as f32 - 0.5
        };
        let iq: Vec<Complex32> = (0..400_000).map(|_| Complex32::new(uniform(), uniform()) * 0.1).collect();
        let (_, audio) = listen(&iq);
        let tail = &audio[audio.len() / 2..];
        let rms = (tail.iter().map(|v| v * v).sum::<f32>() / tail.len() as f32).sqrt();
        let db = 20.0 * rms.max(1e-9).log10();
        eprintln!("idle channel at {db:.1} dBFS");
        assert!(db < -50.0, "idle channel plays at {db:.1} dBFS");
    }

    /// Treble is de-emphasized: a 2.5 kHz tone at the deviation of a 500 Hz
    /// one comes out well under it (750 us de-emphasis and the 3 kHz voice
    /// edge make about -14 dB across that span), where the raw
    /// discriminator played them level.
    #[test]
    fn nfm_is_de_emphasized() {
        let (rate, audio) = listen(&span_with_fm(400_000, &[(500.0, 1.2e3), (2_500.0, 1.2e3)]));
        let tail = &audio[audio.len() / 2..];
        let lo = amp_at(tail, 500.0, f64::from(rate));
        let hi = amp_at(tail, 2_500.0, f64::from(rate));
        let db = 20.0 * (hi / lo).log10();
        eprintln!("2.5 kHz against 500 Hz: {db:.1} dB");
        assert!((-16.0..-12.0).contains(&db), "2.5 kHz sits {db:.1} dB against 500 Hz");
    }

    #[test]
    fn hears_a_channel_inside_the_span() {
        let iq = span_with_fm_tone(200_000);
        let mut rx = SubRx::new(100.1e6, SdrMode::Nfm);
        let mut audio = Vec::new();
        let mut rate = 0;
        for block in iq.chunks(20_000) {
            if let Some((r, a)) = rx.process(block, 1e6, 100e6, 0.0) {
                rate = r;
                audio.extend(a);
            }
        }
        assert_eq!(rate, 47_619, "1 MS/s decimates by 21 toward 48 kHz");
        let tail = &audio[audio.len() / 2..];
        let peak = tail.iter().map(|v| v.unsigned_abs()).max().unwrap_or(0);
        // The leveler steers a steady tone to 0.1 RMS (+12 dB at most): a
        // peak near 0.14 of full scale, 28 000.
        assert!((2_500..8_000).contains(&peak), "peak {peak}");
    }

    #[test]
    fn goes_quiet_outside_the_span_and_says_so() {
        let iq = span_with_fm_tone(20_000);
        let mut rx = SubRx::new(101.0e6, SdrMode::Nfm);
        assert!(rx.process(&iq, 1e6, 100e6, 0.0).is_none());
        assert_eq!(rx.status().note.as_deref(), Some("outside the span"));
    }
}
