//! Voice leveler: a gentle EQ and auto-volume for decoded speech.
//!
//! Transmissions arrive at wildly different levels — one radio is yelling into
//! the mic, the next is across the room — and listening to a scan of them
//! means riding the volume knob. This module evens them out, conservatively:
//!
//! - a high-pass at 180 Hz strips rumble, hum, and any sub-audible tone that
//!   leaked into the voice band (this also keeps a PL tone from counting
//!   toward the measured level on analog channels);
//! - a smoothed RMS envelope drives a gain that pulls speech toward a common
//!   target, limited to ±12 dB so the cure is never worse than the disease;
//! - a **noise guard**: below a speech floor the gain freezes where it is.
//!   Silence, and the muted frames a rejected vocoder burst becomes, are never
//!   boosted — gain can only move while something clearly audible is present.
//!
//! Gain changes are ramped (fast down, slow up) so loud transients are caught
//! quickly without the level audibly pumping, and a final clamp at ±0.95 is
//! the backstop for the few milliseconds a transient can outrun the attack.

use scannerd_dsp::OnePole;

use crate::biquad::Biquad;

/// High-pass corner: below the ~300 Hz voice band, above most rumble.
const HP_HZ: f32 = 180.0;

/// Envelope smoothing time constant. Long enough that syllable-level dynamics
/// do not move the gain; short enough to react to a new speaker.
const ENV_S: f32 = 0.050;

/// Envelope RMS below which the block counts as silence: the gain freezes
/// rather than hunting upward into noise. −40 dB from full scale.
const SPEECH_FLOOR: f32 = 0.010;

/// RMS speech is steered toward, in full-scale units (−20 dBFS).
const TARGET_RMS: f32 = 0.10;

/// Gain limits: ±12 dB. A limit in both directions keeps a mis-measurement
/// from ever producing silence or a blast.
const MIN_GAIN: f32 = 0.25;
const MAX_GAIN: f32 = 4.0;

/// Seconds for the gain to travel its full range downward (loud block
/// arriving) and upward (quiet speaker). Down is fast — clipping is
/// immediate, loudness is merely annoying — up is slow so pauses inside
/// speech do not pump.
const ATTACK_S: f32 = 0.005;
const RELEASE_S: f32 = 0.300;

/// Output never exceeds this magnitude, whatever the envelope says.
const CEILING: f32 = 0.95;

pub struct Leveler {
    hp: Biquad,
    /// One-pole over the squared high-passed signal; its square root is the
    /// level estimate the gain steers by.
    power: OnePole,
    /// Envelope time constant in samples, kept so `reset` can rebuild it.
    power_tau: f32,
    gain: f32,
    attack_step: f32,
    release_step: f32,
}

impl Leveler {
    pub fn new(fs: f64) -> Self {
        let fs = fs as f32;
        let range = MAX_GAIN - MIN_GAIN;
        Self {
            hp: Biquad::highpass(HP_HZ, fs, std::f32::consts::FRAC_1_SQRT_2),
            power: OnePole::new(ENV_S * fs),
            power_tau: ENV_S * fs,
            gain: 1.0,
            attack_step: range / (ATTACK_S * fs).max(1.0),
            release_step: range / (RELEASE_S * fs).max(1.0),
        }
    }

    /// The gain currently being applied, for reporting and tests.
    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// EQ and level `audio` in place.
    pub fn process(&mut self, audio: &mut [f32]) {
        for s in audio.iter_mut() {
            let y = self.hp.process(*s);
            let level = self.power.process(y * y).sqrt();
            // Frozen below the floor: the gain stays exactly where the last
            // audible moment left it, so silence is never turned up.
            let target = if level < SPEECH_FLOOR {
                self.gain
            } else {
                (TARGET_RMS / level).clamp(MIN_GAIN, MAX_GAIN)
            };
            let step = if target < self.gain {
                self.attack_step
            } else {
                self.release_step
            };
            self.gain += (target - self.gain).clamp(-step, step);
            *s = (y * self.gain).clamp(-CEILING, CEILING);
        }
    }

    pub fn reset(&mut self) {
        self.hp.reset();
        self.power = OnePole::new(self.power_tau);
        self.gain = 1.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    const FS: f64 = 16_000.0;

    fn tone(seconds: f32, freq: f32, amp: f32) -> Vec<f32> {
        let n = (seconds * FS as f32) as usize;
        (0..n)
            .map(|i| amp * (TAU * freq * i as f32 / FS as f32).sin())
            .collect()
    }

    fn noise(seconds: f32, amp: f32, seed: u32) -> Vec<f32> {
        let n = (seconds * FS as f32) as usize;
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                amp * ((s >> 8) as f32 / 8388608.0 - 1.0)
            })
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    /// The property the whole module is judged by: quiet noise and silence
    /// must come out no louder than they went in.
    #[test]
    fn silence_and_noise_are_never_amplified() {
        let mut leveler = Leveler::new(FS);

        let mut hush = noise(2.0, 0.003, 7);
        let before = rms(&hush);
        leveler.process(&mut hush);
        let after = rms(&hush);
        assert!(
            after <= before * 1.05,
            "quiet noise grew from {before:.5} to {after:.5}"
        );
        assert!(
            leveler.gain() <= 1.0 + 1e-6,
            "gain rose to {} on noise",
            leveler.gain()
        );

        let mut zeros = vec![0.0f32; 16_000];
        Leveler::new(FS).process(&mut zeros);
        assert!(zeros.iter().all(|&v| v == 0.0));
    }

    /// Speech well above the floor but well under the target is lifted.
    #[test]
    fn quiet_speech_is_lifted_toward_the_target() {
        let mut leveler = Leveler::new(FS);
        let mut speech = tone(3.0, 1000.0, 0.03);
        leveler.process(&mut speech);
        let settled = rms(&speech[16_000..]);
        assert!(
            settled > 0.05,
            "quiet speech only reached {settled:.4} after a second of settling"
        );
        assert!(
            settled < 0.15,
            "quiet speech overshot the target: {settled:.4}"
        );
        assert!(leveler.gain() > 1.5, "gain only reached {}", leveler.gain());
    }

    /// A hot transmission is pulled down quickly, and nothing ever clips.
    #[test]
    fn loud_speech_is_ducked_and_never_clips() {
        let mut leveler = Leveler::new(FS);
        let mut speech = tone(2.0, 1000.0, 0.9);
        leveler.process(&mut speech);
        let peak = speech.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak <= 0.95 + 1e-6, "output clipped at {peak}");
        let settled = rms(&speech[8_000..]);
        assert!(settled < 0.25, "loud speech was not ducked: {settled:.4}");
        assert!(leveler.gain() < 0.5, "gain only fell to {}", leveler.gain());
    }

    /// A pause inside a transmission must never turn into audible noise: the
    /// output decays to silence and stays there. The gain itself releases
    /// upward while the envelope's decay tail is still above the floor —
    /// ordinary AGC release — but once the signal is truly gone the guard
    /// freezes it, and zero in remains zero out.
    #[test]
    fn a_pause_never_becomes_audible_noise() {
        let mut leveler = Leveler::new(FS);
        let mut loud = tone(1.0, 1000.0, 0.9);
        leveler.process(&mut loud);

        let mut pause = vec![0.0f32; 16_000];
        leveler.process(&mut pause);
        let tail = rms(&pause[8_000..]);
        assert!(tail < 1e-4, "silence came out at {tail:.6}");
        assert!(leveler.gain() <= MAX_GAIN + 1e-6);

        // And speech after the pause is still levelled, so the freeze did not
        // wedge the gain anywhere silly.
        let mut quiet = tone(2.0, 1000.0, 0.03);
        leveler.process(&mut quiet);
        let settled = rms(&quiet[16_000..]);
        assert!(
            settled > 0.04,
            "post-pause speech only reached {settled:.4}"
        );
    }

    /// The EQ half: sub-voice-band energy is cut, speech-band energy is not.
    /// Levels are kept low enough that the gain stays near 1.0 for both, so
    /// the comparison measures the filter rather than the AGC.
    #[test]
    fn rumble_is_cut_speech_is_not() {
        let mut rumble = tone(0.5, 60.0, 0.02);
        let mut speech = tone(0.5, 1000.0, 0.02);
        Leveler::new(FS).process(&mut rumble);
        Leveler::new(FS).process(&mut speech);
        let low = rms(&rumble[4_000..]);
        let mid = rms(&speech[4_000..]);
        assert!(
            low < mid * 0.25,
            "60 Hz survived at {low:.5} against 1 kHz at {mid:.5}"
        );
    }

    /// Gain is ramped per-sample: no step large enough to be a click.
    #[test]
    fn the_gain_ramps_rather_than_switching() {
        let mut leveler = Leveler::new(FS);
        let mut block = tone(0.1, 1000.0, 1.0);
        leveler.process(&mut block);
        let max_step = (MAX_GAIN - MIN_GAIN) / (ATTACK_S * FS as f32) + 1e-6;
        for pair in block.windows(2) {
            // Amplitude change between adjacent samples beyond what the tone
            // itself does would mean the gain jumped.
            assert!(
                (pair[1] - pair[0]).abs() <= 1.0 * (TAU * 1000.0 / FS as f32) + max_step + 1e-6,
                "output jumped from {} to {}",
                pair[0],
                pair[1]
            );
        }
    }
}
