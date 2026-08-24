//! Narrowband FM demodulation for land-mobile voice.
//!
//! The discriminator itself is the standard delay-line form — the same three
//! lines already sitting inline in hfscan's RTTY decoder, lifted out because
//! here it is the product rather than an implementation detail of something
//! else.
//!
//! What surrounds it is specific to land mobile. The output is split into two
//! paths: **voice**, high-passed at 300 Hz, and **sub-audible**, low-passed at
//! 250 Hz, which is where CTCSS lives. Splitting at the demodulator rather than
//! filtering later means the tone decoder sees the tone without speech on top of
//! it, and the audio never carries an audible 100 Hz hum.

use crate::biquad::Cascade;
use num_complex::Complex32;
use scannerd_dsp::OnePole;

/// Peak deviation of a 12.5 kHz land-mobile channel, in Hz.
///
/// Used to scale the discriminator so full deviation reads ±1.0. Narrowband
/// channels are 2.5 kHz; the 5 kHz figure belongs to wideband 25 kHz channels.
pub const NARROW_DEVIATION_HZ: f32 = 2500.0;

/// Occupied IF bandwidth of narrowband FM. Carson's rule:
/// `2 × (2.5 kHz deviation + 3 kHz audio) = 11 kHz`. The 12.5 kHz figure is
/// the channel *allocation*, not what the signal fills.
pub const NBFM_BANDWIDTH_HZ: f64 = 11_000.0;

/// Voice low-pass after de-emphasis. Land-mobile speech stops at 3 kHz;
/// everything above that is discriminator hiss.
pub const VOICE_LOWPASS_HZ: f32 = 3_000.0;

/// Land-mobile de-emphasis time constant, in seconds.
pub const DEEMPHASIS_TAU: f32 = 750e-6;

/// Where the noise-detection band starts.
///
/// Above anything voice puts there, and below the channel filter's corner, so
/// the band contains only what FM quieting acts on. A carrier suppresses this
/// region hard; its absence fills it with full-scale discriminator noise, which
/// makes it a far faster and more reliable carrier detector than RF power.
pub const NOISE_CORNER_HZ: f32 = 4000.0;

/// Where voice starts, and where CTCSS is required to have stopped.
pub const VOICE_CORNER_HZ: f32 = 300.0;
/// Where the sub-audible path stops, above every standard CTCSS tone (250.3 Hz).
pub const SUBAUDIBLE_CORNER_HZ: f32 = 260.0;

pub struct NbfmDemod {
    prev: Complex32,
    scale: f32,
    deviation_hz: f32,
    deemph: OnePole,
    voice_hp: Cascade,
    voice_lp: Cascade,
    sub_lp: Cascade,
    noise_hp: Cascade,
    noise_env: OnePole,
}

/// One block of demodulated audio, split by band.
#[derive(Default)]
pub struct Demodulated {
    /// Voice audio, nominally ±1.0 at full deviation.
    pub voice: Vec<f32>,
    /// Raw discriminator audio (±1.0 at full deviation) before de-emphasis and filtering.
    pub raw: Vec<f32>,
    /// The sub-audible band, where CTCSS lives.
    pub subaudible: Vec<f32>,
    /// Per-sample envelope of the high-frequency noise band.
    ///
    /// Near zero while a carrier is present, large when it is not. This is what
    /// the noise gate keys on.
    pub noise: Vec<f32>,
    /// Mean carrier offset over this block, in Hz.
    ///
    /// An FM discriminator's DC level *is* the frequency error: with the
    /// receiver on frequency, symmetric modulation averages to zero. Anything
    /// else is the amount by which the channel is mistuned, and mistuning is
    /// what turns clean voice into muffled low-frequency mush.
    pub mean_offset_hz: f32,
}

impl Demodulated {
    pub fn clear(&mut self) {
        self.voice.clear();
        self.raw.clear();
        self.subaudible.clear();
        self.noise.clear();
        self.mean_offset_hz = 0.0;
    }
}

impl NbfmDemod {
    pub fn new(fs: f64) -> Self {
        Self::with_deviation(fs, NARROW_DEVIATION_HZ)
    }

    pub fn with_deviation(fs: f64, deviation_hz: f32) -> Self {
        let fs32 = fs as f32;
        Self {
            prev: Complex32::new(0.0, 0.0),
            // arg() of the product spans ±π for ±fs/2 of instantaneous
            // frequency, so this maps full deviation onto ±1.0.
            scale: fs32 / (std::f32::consts::TAU * deviation_hz),
            deviation_hz,
            deemph: OnePole::new(DEEMPHASIS_TAU * fs32),
            voice_hp: Cascade::highpass(VOICE_CORNER_HZ, fs32, 2),
            voice_lp: Cascade::lowpass(VOICE_LOWPASS_HZ, fs32, 2),
            sub_lp: Cascade::lowpass(SUBAUDIBLE_CORNER_HZ, fs32, 2),
            noise_hp: Cascade::highpass(NOISE_CORNER_HZ, fs32, 2),
            // ~3 ms: fast enough to catch a dropout inside a syllable, slow
            // enough not to chatter on the noise's own peaks.
            noise_env: OnePole::new(0.003 * fs32),
        }
    }

    pub fn process(&mut self, input: &[Complex32], out: &mut Demodulated) {
        out.clear();
        out.voice.reserve(input.len());
        out.raw.reserve(input.len());
        out.subaudible.reserve(input.len());
        out.noise.reserve(input.len());
        let mut sum = 0.0f64;
        for &x in input {
            let d = x * self.prev.conj();
            self.prev = x;
            // A zero sample has no defined angle; arg() would return 0 anyway,
            // but going through it wastes an atan2 on silence.
            let raw = if d.norm_sqr() > 0.0 {
                d.arg() * self.scale
            } else {
                0.0
            };
            sum += f64::from(raw);
            out.raw.push(raw);
            out.noise
                .push(self.noise_env.process(self.noise_hp.process(raw).abs()));
            out.subaudible.push(self.sub_lp.process(raw));
            out.voice.push(
                self.voice_lp
                    .process(self.voice_hp.process(self.deemph.process(raw))),
            );
        }
        if !input.is_empty() {
            out.mean_offset_hz = (sum / input.len() as f64) as f32 * self.deviation_hz;
        }
    }

    /// Instantaneous frequency in Hz (positive = above the LO).
    pub fn discriminator_hz(&mut self, input: &[Complex32], out: &mut Vec<f32>) {
        out.clear();
        out.reserve(input.len());
        for &x in input {
            let d = x * self.prev.conj();
            self.prev = x;
            let hz = if d.norm_sqr() > 0.0 {
                d.arg() * self.scale * self.deviation_hz
            } else {
                0.0
            };
            out.push(hz);
        }
    }

    pub fn reset(&mut self) {
        self.prev = Complex32::new(0.0, 0.0);
        self.voice_hp.reset();
        self.voice_lp.reset();
        self.sub_lp.reset();
        self.noise_hp.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    /// Synthesise an FM carrier: a `tone_hz` modulation at `dev_hz` deviation.
    fn fm(n: usize, fs: f32, tone_hz: f32, dev_hz: f32) -> Vec<Complex32> {
        let mut phase = 0.0f32;
        (0..n)
            .map(|i| {
                let m = (TAU * tone_hz * i as f32 / fs).sin();
                phase += TAU * dev_hz * m / fs;
                Complex32::new(phase.cos(), phase.sin())
            })
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    /// Full deviation must produce a known, derivable level, which is what lets
    /// the audio path run without an AGC in front of it.
    ///
    /// The expected figure is not "full scale". A sine at ±1.0 peak has RMS
    /// 0.707, and this test tone is *not* pre-emphasised, so de-emphasis
    /// legitimately attenuates it: the one-pole corner is
    /// `1 / (2π · 750 µs) = 212 Hz`, giving |H| = 0.209 at 1 kHz. Real traffic
    /// arrives pre-emphasised and comes back out flat.
    #[test]
    fn full_deviation_reads_the_predicted_level() {
        let fs = 16_000.0;
        let mut d = NbfmDemod::new(fs as f64);
        let mut out = Demodulated::default();
        d.process(&fm(8192, fs, 1000.0, NARROW_DEVIATION_HZ), &mut out);
        let got = rms(&out.voice[2048..]);
        let want = std::f32::consts::FRAC_1_SQRT_2 * 0.209;
        assert!(
            (got / want - 1.0).abs() < 0.15,
            "voice RMS {got:.4}, expected about {want:.4}"
        );
    }

    /// De-emphasis has to be the standard 750 µs, not merely "some low-pass":
    /// get the corner wrong and every recording is muffled or hissy.
    #[test]
    fn deemphasis_has_the_standard_corner() {
        let fs = 16_000.0;
        let mut out = Demodulated::default();
        let mut level = |tone| {
            let mut d = NbfmDemod::new(fs as f64);
            d.process(&fm(16384, fs, tone, NARROW_DEVIATION_HZ), &mut out);
            rms(&out.voice[4096..])
        };
        // One pole at 212 Hz: |H| is 0.209 at 1 kHz and 0.108 at 2 kHz
        // (ratio 1.93). The 3 kHz voice LPF (two Q=0.707 sections) then
        // takes another ~1.6 dB off 2 kHz, so the combined ratio is ~2.18.
        let ratio = level(1000.0) / level(2000.0);
        assert!(
            (ratio / 2.18 - 1.0).abs() < 0.1,
            "1 kHz / 2 kHz ratio {ratio:.2}, expected about 2.18"
        );
    }

    /// The split is the point of the module: a CTCSS tone must appear in the
    /// sub-audible path and be absent from the voice path.
    #[test]
    fn a_ctcss_tone_lands_in_the_subaudible_path_only() {
        let fs = 16_000.0;
        let mut d = NbfmDemod::new(fs as f64);
        let mut out = Demodulated::default();
        // CTCSS is transmitted at low deviation, typically a fifth of voice.
        d.process(&fm(32768, fs, 192.8, NARROW_DEVIATION_HZ * 0.2), &mut out);
        let sub = rms(&out.subaudible[8192..]);
        let voice = rms(&out.voice[8192..]);
        assert!(
            sub > 0.05,
            "tone should reach the sub-audible path, got {sub:.4}"
        );
        assert!(
            voice < sub * 0.2,
            "tone leaked into voice: sub {sub:.4}, voice {voice:.4}"
        );
    }

    /// And the converse, or the tone decoder would be looking at speech.
    #[test]
    fn speech_stays_out_of_the_subaudible_path() {
        let fs = 16_000.0;
        let mut d = NbfmDemod::new(fs as f64);
        let mut out = Demodulated::default();
        d.process(&fm(32768, fs, 1200.0, NARROW_DEVIATION_HZ), &mut out);
        let sub = rms(&out.subaudible[8192..]);
        let voice = rms(&out.voice[8192..]);
        assert!(
            sub < voice * 0.2,
            "speech leaked: sub {sub:.4}, voice {voice:.4}"
        );
    }

    /// Hiss above the voice band must not reach the listener. A 5 kHz tone is
    /// well above 3 kHz speech and well below the 8 kHz Nyquist, so if it
    /// survives the low-pass the filter is not doing its job.
    #[test]
    fn hiss_above_the_voice_band_is_rejected() {
        let fs = 16_000.0;
        let mut out = Demodulated::default();
        let mut level = |tone| {
            let mut d = NbfmDemod::new(fs as f64);
            d.process(&fm(16384, fs, tone, NARROW_DEVIATION_HZ), &mut out);
            rms(&out.voice[4096..])
        };
        let speech = level(1000.0);
        let hiss = level(5000.0);
        assert!(
            hiss < speech * 0.15,
            "5 kHz hiss leaked: speech {speech:.4}, hiss {hiss:.4}"
        );
    }

    #[test]
    fn an_unmodulated_carrier_demodulates_to_near_silence() {
        let fs = 16_000.0;
        let mut d = NbfmDemod::new(fs as f64);
        let mut out = Demodulated::default();
        d.process(&fm(8192, fs, 0.0, 0.0), &mut out);
        assert!(rms(&out.voice[2048..]) < 0.01);
    }

    #[test]
    fn deviation_scales_the_output_proportionally() {
        let fs = 16_000.0;
        let mut out = Demodulated::default();
        let mut full = NbfmDemod::new(fs as f64);
        full.process(&fm(16384, fs, 1000.0, NARROW_DEVIATION_HZ), &mut out);
        let a = rms(&out.voice[4096..]);
        let mut half = NbfmDemod::new(fs as f64);
        half.process(&fm(16384, fs, 1000.0, NARROW_DEVIATION_HZ * 0.5), &mut out);
        let b = rms(&out.voice[4096..]);
        assert!(
            (a / b - 2.0).abs() < 0.15,
            "half deviation should be half amplitude, got ratio {:.3}",
            a / b
        );
    }
}
