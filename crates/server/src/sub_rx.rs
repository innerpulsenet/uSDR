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
use scannerd_engine::nbfm::NbfmDemod;
use scannerd_engine::ssb::{Sideband, SsbDemod};
use scannerd_engine::stereo::{WFM_AUDIO_SCALE, WFM_DC_TAU_S};
use serde::Serialize;

/// Narrowband FM deviation the audio is scaled to: 5 kHz peak plays at full
/// scale, the 2.5 kHz of a narrowbanded channel at half.
const NFM_DEVIATION_HZ: f32 = 5_000.0;
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
    /// Why it is silent, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

enum Demod {
    Fm { fm: NbfmDemod, disc: Vec<f32>, dc: OnePole },
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
                Demod::Ssb { ssb: SsbDemod::with_passband(fs, side, lo.abs().min(hi.abs()), lo.abs().max(hi.abs())) }
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
                fm: NbfmDemod::with_deviation(fs, NFM_DEVIATION_HZ),
                disc: Vec::new(),
                // A 50 ms one-pole tracks the carrier offset out of the audio.
                dc: OnePole::new(0.05 * fs as f32),
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
            Demod::Fm { fm, disc, dc } => {
                fm.discriminator_hz(&self.iq, disc);
                for &hz in disc.iter() {
                    let v = hz / NFM_DEVIATION_HZ;
                    self.audio.push((v - dc.process(v)).clamp(-1.0, 1.0));
                }
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
        let fs = 1e6;
        let mut phase = 0.0f64;
        (0..n)
            .map(|i| {
                let t = i as f64 / fs;
                let f = 100e3 + 2.5e3 * (2.0 * std::f64::consts::PI * 1e3 * t).sin();
                phase += 2.0 * std::f64::consts::PI * f / fs;
                Complex32::new(phase.cos() as f32 * 0.3, phase.sin() as f32 * 0.3)
            })
            .collect()
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
        // 2.5 kHz of a 5 kHz full scale: about half of 28 000.
        assert!((10_000..18_000).contains(&peak), "peak {peak}");
    }

    #[test]
    fn goes_quiet_outside_the_span_and_says_so() {
        let iq = span_with_fm_tone(20_000);
        let mut rx = SubRx::new(101.0e6, SdrMode::Nfm);
        assert!(rx.process(&iq, 1e6, 100e6, 0.0).is_none());
        assert_eq!(rx.status().note.as_deref(), Some("outside the span"));
    }
}
