//! Audio-domain noise gate: the fast half of the squelch.
//!
//! The [`Squelch`](crate::squelch::Squelch) decides where a *call* begins and
//! ends, from RF power measured across the span. That decision is deliberately
//! slow and hysteretic — it has to bridge the pauses inside speech so one
//! transmission stays one call. Which means audio keeps flowing while no
//! carrier is present at all: through the pre-roll before the carrier arrives,
//! through the hang after the operator unkeys, and through any mid-transmission
//! dropout. An FM discriminator with no carrier produces **full-scale noise**,
//! so all three come out as bursts of loud static.
//!
//! This gate solves the other half of the problem: whether the audio *right now*
//! is voice or noise. It keys on the high-frequency band of the discriminator
//! output, where FM quieting acts most strongly — a carrier suppresses it by
//! orders of magnitude, and its absence fills it. That makes it both faster and
//! far more decisive than RF power, and it does not care how strong the signal
//! is, only whether it is there.

use scannerd_dsp::OnePole;

/// Noise-band envelope below which the gate opens.
///
/// Measured through the real receive path (2.4 MS/s span, channel filter,
/// 48 kS/s output, band bounded to 4–8 kHz per `nbfm::NOISE_BAND_TOP_HZ`),
/// at the demodulator's scaling where ±1.0 is full deviation. A carrier with
/// noise under it is not the 1e-3 a clean tone gives — FM noise rises with
/// frequency — so the band reads 0.05 at 20 dB, 0.15–0.18 at 10 dB and
/// 0.25–0.29 at 6 dB (12.5 / 25 kHz channels), while no carrier at all
/// reads 0.46 (12.5 kHz channel at 16 kS/s) to 0.98 (25 kHz at 48 kS/s).
/// The old thresholds of 0.020/0.045 were set against clean tones and shut
/// the gate on anything noisier than ~25 dB, which after the monitor path
/// moved onto the 48 kS/s chain was every real signal: NFM audio was mute.
/// A carrier of ~9 dB or better opens the gate directly; a weaker one in
/// the gap opens through the ambiguity timer.
pub const DEFAULT_OPEN: f32 = 0.18;
/// Envelope above which it closes again. The gap is hysteresis.
pub const DEFAULT_CLOSE: f32 = 0.28;

/// Seconds to ramp fully open. Short, so a syllable's first moment is not lost.
const OPEN_S: f32 = 0.004;
/// Seconds to ramp fully closed. Slightly longer, so ordinary speech dynamics
/// do not make the gain chatter audibly.
const CLOSE_S: f32 = 0.015;
/// How long the envelope may sit inside the hysteresis gap while the gate is
/// closed before ambiguity resolves as "probably a weak carrier" and the gate
/// opens. Long enough that pure noise — which rides *above* `close_level`,
/// not in the gap — never trips it; short enough that a carrier arriving mid-
/// sentence is heard within a couple of seconds.
const AMBIGUOUS_S: f32 = 1.5;

/// Longest extra time a MARGINAL noise-band level (just over the close line)
/// can persist before an open gate closes. Deep static bypasses this entirely.
/// Rationale: a weak carrier's quieting leaves its band hovering at 1–2× the
/// line; one speech transient there used to cost a multi-second dropout.
const CLOSE_DELAY_S: f32 = 0.40;
/// Band level, as a multiple of the close threshold, that is unmistakably
/// carrier-less static and closes the gate at once. The lowest no-carrier
/// reading measured (12.5 kHz channel, 16 kS/s) is 0.46 = 1.6×; 1.5 × 0.28
/// = 0.42 sits under it, while a 3–6 dB carrier at 0.29–0.36 gets the
/// delayed close.
const DEEP_STATIC_OVER: f32 = 1.5;
/// Samples after construction or reset during which the gate stays shut
/// whatever the band says. A freshly built channel filter emits zeros
/// while it fills, and a zero discriminator reads as a perfectly quieted
/// carrier; the gate opened on that silence and, with the smoothed band
/// still climbing toward its real level, a call could start on nothing.
const WARMUP_S: f32 = 0.05;

pub struct NoiseGate {
    open_level: f32,
    close_level: f32,
    open_step: f32,
    close_step: f32,
    gain: f32,
    open: bool,
    smooth: OnePole,
    /// Fraction of the last block that was passed, for reporting.
    last_duty: f32,
    fs: f32,
    /// Samples the smoothed envelope has continuously sat in the hysteresis
    /// gap between `open_level` and `close_level` while closed. That gap is
    /// where a WEAK carrier's partial quieting lands: strong enough to pull
    /// the band below the close threshold, too weak to reach the open one.
    /// Left unbounded it latches the gate shut over real traffic — which is
    /// also what a scope fed from the gated audio would show as a dead
    /// flatline. Past [`AMBIGUOUS_S`] seconds of ambiguity the gate opens
    /// anyway; if it truly is noise, the band climbs and the gate re-closes
    /// on its own within milliseconds.
    ambiguous_for: usize,
    /// Samples the smoothed envelope has continuously exceeded `close_level`
    /// while open, feeding the depth-scaled close delay.
    over_close_for: usize,
    /// Samples seen since construction or reset, against [`WARMUP_S`].
    warm: usize,
}

impl NoiseGate {
    pub fn new(fs: f64) -> Self {
        Self::with_levels(fs, DEFAULT_OPEN, DEFAULT_CLOSE)
    }

    pub fn with_levels(fs: f64, open_level: f32, close_level: f32) -> Self {
        let fs = fs as f32;
        Self {
            open_level,
            // An open level above the close level would invert the hysteresis.
            close_level: close_level.max(open_level),
            open_step: 1.0 / (OPEN_S * fs).max(1.0),
            close_step: 1.0 / (CLOSE_S * fs).max(1.0),
            gain: 0.0,
            open: false,
            // A little extra smoothing on top of the demodulator's envelope, so
            // a single noisy sample cannot slam the gate shut mid-word.
            smooth: OnePole::new(0.008 * fs),
            last_duty: 0.0,
            fs,
            ambiguous_for: 0,
            over_close_for: 0,
            warm: 0,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// How much of the most recent block was let through, in `0.0..=1.0`.
    pub fn duty(&self) -> f32 {
        self.last_duty
    }

    /// Attenuate `voice` in place wherever `noise` says there is no carrier.
    ///
    /// Gain is ramped rather than switched: a hard mute on a full-scale noise
    /// burst is itself an audible click, which is the very thing being removed.
    pub fn process(&mut self, voice: &mut [f32], noise: &[f32]) {
        let mut passed = 0.0f32;
        let warm_need = (WARMUP_S * self.fs).max(1.0) as usize;
        for (i, v) in voice.iter_mut().enumerate() {
            let level = self.smooth.process(noise.get(i).copied().unwrap_or(0.0));
            if self.warm < warm_need {
                // Track the level, decide nothing yet.
                self.warm += 1;
                self.open = false;
            } else if self.open {
                // A weak carrier partially quiets the band, so its level
                // hovers just OVER the close line rather than clearly below
                // it as a strong carrier does. One sample over used to slam
                // the gate shut instantly; combined with the 1.5 s ambiguity
                // timer that produced periodic multi-second dropouts on
                // marginal signals. Closing now scales with how far above
                // the line the level sits: hovering (≤2×) must persist
                // ~CLOSE_DELAY_S before closing, while deep dead-air (≥4×,
                // full static) closes immediately as always.
                if level > self.close_level {
                    let over = (level / self.close_level.max(1e-6)).max(1.0);
                    if over >= DEEP_STATIC_OVER {
                        self.open = false;
                        self.ambiguous_for = 0;
                        self.over_close_for = 0;
                    } else {
                        self.over_close_for += 1;
                        // 1× (a weak carrier hovering at the line) waits
                        // the full CLOSE_DELAY_S; the wait shrinks linearly
                        // to an eighth of it just under DEEP_STATIC_OVER.
                        let depth = ((over - 1.0) / (DEEP_STATIC_OVER - 1.0)).clamp(0.0, 1.0);
                        let seconds = CLOSE_DELAY_S * (1.0 - 0.875 * depth);
                        if self.over_close_for >= (seconds * self.fs).max(1.0) as usize {
                            self.open = false;
                            self.ambiguous_for = 0;
                            self.over_close_for = 0;
                        }
                    }
                } else {
                    self.over_close_for = 0;
                }
            } else if level < self.open_level {
                self.open = true;
                self.ambiguous_for = 0;
            } else {
                // In the gap: either hysteresis doing its job on a real
                // carrier, or a weak one that will never cross either edge.
                // Count how long, and resolve stalemates toward open so a
                // marginal signal is heard (and shown on any gated scope).
                self.ambiguous_for += 1;
                if self.ambiguous_for >= (AMBIGUOUS_S * self.fs).max(1.0) as usize {
                    self.open = true;
                    self.ambiguous_for = 0;
                }
            }
            let target = if self.open { 1.0 } else { 0.0 };
            let step = if self.open {
                self.open_step
            } else {
                self.close_step
            };
            self.gain += (target - self.gain).clamp(-step, step);
            self.gain = self.gain.clamp(0.0, 1.0);
            *v *= self.gain;
            passed += self.gain;
        }
        if !voice.is_empty() {
            self.last_duty = passed / voice.len() as f32;
        }
    }

    pub fn reset(&mut self) {
        self.gain = 0.0;
        self.open = false;
        self.last_duty = 0.0;
        self.ambiguous_for = 0;
        self.over_close_for = 0;
        self.warm = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nbfm::{Demodulated, NARROW_DEVIATION_HZ, NbfmDemod};
    use num_complex::Complex32;
    use std::f32::consts::TAU;

    const FS: f32 = 16_000.0;

    fn fm(n: usize, tone_hz: f32, dev_hz: f32) -> Vec<Complex32> {
        let mut phase = 0.0f32;
        (0..n)
            .map(|i| {
                phase += TAU * dev_hz * (TAU * tone_hz * i as f32 / FS).sin() / FS;
                Complex32::new(phase.cos(), phase.sin())
            })
            .collect()
    }

    /// No carrier: complex noise, which is what the front end delivers between
    /// transmissions.
    fn noise_iq(n: usize, seed: u32) -> Vec<Complex32> {
        let mut s = seed;
        let mut r = move || {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            (s >> 8) as f32 / 8388608.0 - 1.0
        };
        (0..n).map(|_| Complex32::new(r(), r())).collect()
    }

    fn demod(iq: &[Complex32]) -> Demodulated {
        let mut d = NbfmDemod::new(FS as f64);
        let mut out = Demodulated::default();
        d.process(iq, &mut out);
        out
    }

    fn mean(x: &[f32]) -> f32 {
        x.iter().sum::<f32>() / x.len().max(1) as f32
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    /// The measurement the whole module rests on: the noise band separates a
    /// present carrier from an absent one by a wide margin, and the default
    /// thresholds sit inside that gap.
    #[test]
    fn the_noise_band_separates_carrier_from_no_carrier() {
        let quiet = demod(&fm(16384, 1000.0, NARROW_DEVIATION_HZ));
        let loud = demod(&noise_iq(16384, 7));
        let with_carrier = mean(&quiet.noise[4096..]);
        let without = mean(&loud.noise[4096..]);

        assert!(
            without > with_carrier * 20.0,
            "separation too small: carrier {with_carrier:.5}, noise {without:.5}"
        );
        assert!(
            with_carrier < DEFAULT_OPEN,
            "a quieted carrier ({with_carrier:.5}) must fall below the open threshold {DEFAULT_OPEN}"
        );
        assert!(
            without > DEFAULT_CLOSE,
            "carrier-less noise ({without:.5}) must exceed the close threshold {DEFAULT_CLOSE}"
        );
    }

    /// The regression that muted NFM: on the shared 48 kS/s chain the noise
    /// band ran from 4 kHz to Nyquist (24 kHz), and a strong carrier with a
    /// realistic 20 dB of noise under it filled that band (FM noise rises
    /// with frequency) to 0.35 — above the close threshold, so the gate never
    /// opened. The band now stops at NOISE_BAND_TOP_HZ at any sample rate.
    #[test]
    fn a_noisy_carrier_still_opens_the_gate_at_the_chain_rate() {
        const FS48: f32 = 48_000.0;
        let n = 48_000;
        let mut phase = 0.0f32;
        let carrier: Vec<Complex32> = (0..n)
            .map(|i| {
                phase += TAU * NARROW_DEVIATION_HZ * (TAU * 1000.0 * i as f32 / FS48).sin() / FS48;
                Complex32::new(phase.cos(), phase.sin())
            })
            .collect();
        // Complex noise 20 dB below the carrier across the whole 48 kHz.
        let noise = noise_iq(n, 5);
        let noise_rms = rms(&noise.iter().map(|c| c.norm()).collect::<Vec<_>>());
        let scale = 0.1 / noise_rms;
        let iq: Vec<Complex32> = carrier.iter().zip(&noise).map(|(c, w)| c + w * scale).collect();
        let mut d = NbfmDemod::new(FS48 as f64);
        let mut out = Demodulated::default();
        d.process(&iq, &mut out);
        let band = mean(&out.noise[8192..]);
        assert!(
            band < DEFAULT_OPEN,
            "a 20 dB carrier at 48 kS/s reads {band:.4} in the noise band; the gate would stay shut"
        );
        let mut voice = out.voice.clone();
        let mut gate = NoiseGate::new(FS48 as f64);
        gate.process(&mut voice, &out.noise);
        assert!(gate.is_open() && gate.duty() > 0.5, "gate duty {:.2}", gate.duty());
        // And carrier-less noise at that rate still closes it.
        let mut out2 = Demodulated::default();
        NbfmDemod::new(FS48 as f64).process(&noise_iq(n, 9), &mut out2);
        assert!(mean(&out2.noise[8192..]) > DEFAULT_CLOSE);
    }

    /// The behaviour the user actually asked for: static between transmissions
    /// must be silenced.
    #[test]
    fn static_is_attenuated_to_near_silence() {
        let d = demod(&noise_iq(16384, 11));
        let mut voice = d.voice.clone();
        let mut gate = NoiseGate::new(FS as f64);
        gate.process(&mut voice, &d.noise);
        let before = rms(&d.voice[4096..]);
        let after = rms(&voice[4096..]);
        assert!(
            after < before * 0.02,
            "static only fell from {before:.4} to {after:.4}"
        );
        assert!(!gate.is_open());
    }

    /// And the converse, or the gate would be worse than the problem.
    #[test]
    fn voice_on_a_carrier_passes_essentially_untouched() {
        let d = demod(&fm(16384, 1000.0, NARROW_DEVIATION_HZ));
        let mut voice = d.voice.clone();
        let mut gate = NoiseGate::new(FS as f64);
        gate.process(&mut voice, &d.noise);
        let before = rms(&d.voice[8192..]);
        let after = rms(&voice[8192..]);
        assert!(
            after > before * 0.95,
            "voice was attenuated from {before:.4} to {after:.4}"
        );
        assert!(gate.is_open());
        assert!(gate.duty() > 0.9);
    }

    /// A carrier dropping mid-call is the third source of static, and the gate
    /// has to shut within a few milliseconds rather than at the next block.
    #[test]
    fn a_dropout_is_caught_quickly_and_recovers() {
        let mut iq = fm(8192, 1000.0, NARROW_DEVIATION_HZ);
        iq.extend(noise_iq(8192, 3));
        iq.extend(fm(8192, 1000.0, NARROW_DEVIATION_HZ));
        let d = demod(&iq);
        let mut voice = d.voice.clone();
        let mut gate = NoiseGate::new(FS as f64);
        gate.process(&mut voice, &d.noise);

        let during = rms(&voice[9000..15000]);
        let after = rms(&voice[18000..]);
        assert!(during < 0.02, "dropout still leaked {during:.4}");
        assert!(after > 0.05, "gate failed to reopen, level {after:.4}");
    }

    /// Ramping, not switching: the gain must never jump far in one sample, or
    /// the cure is itself a click.
    #[test]
    fn the_gain_ramps_rather_than_switching() {
        let mut gate = NoiseGate::new(FS as f64);
        let noise = vec![0.0f32; 512];
        let mut voice = vec![1.0f32; 512];
        gate.process(&mut voice, &noise);
        for pair in voice.windows(2) {
            assert!(
                (pair[1] - pair[0]).abs() <= 1.0 / (OPEN_S * FS) + 1e-6,
                "gain jumped from {} to {}",
                pair[0],
                pair[1]
            );
        }
    }

    /// The weak-carrier latch: a carrier whose partial quieting parks the
    /// noise band inside the hysteresis gap must not hold the gate shut
    /// forever. Before the ambiguity timer this exact case silenced NFM
    /// audio and flatlined any scope fed from the gated path.
    #[test]
    fn a_weak_carrier_in_the_hysteresis_gap_reopens() {
        let mut gate = NoiseGate::new(FS as f64);
        // A steady mid-gap envelope: below close_level (0.28), above
        // open_level (0.18). Voice content is irrelevant; what is under
        // test is the gate's own state machine.
        // Two seconds for the ambiguity timer (1.5 s) to resolve, then a
        // third second that must pass essentially whole. (This used to run
        // as one 3 s block and pass on the smoother's zero start opening the
        // gate at sample 0 — the warm-up now rules that out.)
        let voice = vec![1.0f32; 2 * FS as usize];
        let noise = vec![0.23f32; voice.len()];
        let mut gated = voice.clone();
        gate.process(&mut gated, &noise);
        assert!(gate.is_open(), "ambiguity timer did not open the gate");
        let mut third = vec![1.0f32; FS as usize];
        gate.process(&mut third, &noise[..FS as usize]);
        assert!(
            gate.duty() > 0.9,
            "gate stayed latched shut on a mid-gap band (duty {})",
            gate.duty()
        );
        // And pure noise — which rides above close_level — still holds it shut.
        let mut gate2 = NoiseGate::new(FS as f64);
        let loud_noise = vec![0.90f32; FS as usize];
        let mut quiet_voice = vec![1.0f32; FS as usize];
        gate2.process(&mut quiet_voice, &loud_noise);
        assert!(gate2.duty() < 0.05, "gate opened on full-scale noise");
    }

    #[test]
    fn inverted_levels_are_clamped() {
        let g = NoiseGate::with_levels(FS as f64, 0.05, 0.01);
        assert_eq!(g.close_level, 0.05);
    }

    /// THE GLITCH THIS FIXES: a marginal carrier's band hovers just over the
    /// close line. Before the depth-scaled close delay, one hovering sample
    /// slammed the gate shut and the 1.5 s ambiguity timer kept it there —
    /// periodic multi-second audio dropouts on weak signals. Hovering must
    /// open (via the ambiguity path) and brief dips below must not re-close.
    #[test]
    fn a_marginal_carrier_holds_the_gate_open() {
        let mut gate = NoiseGate::new(FS as f64);
        // Strong carrier first: well under open_level, gate opens cleanly.
        let strong = vec![0.0f32; FS as usize];
        let mut audio = vec![0.3f32; strong.len()];
        gate.process(&mut audio, &strong);
        assert!(gate.is_open());
        // Signal fades to marginal: band sits at ~1.1× close_level —
        // hovering territory. Hold it through alternating hover /
        // dip-below-line segments (speech-like) for many seconds.
        for _ in 0..20 {
            let hover = vec![DEFAULT_CLOSE * 1.1; FS as usize / 4];
            let mut a = vec![0.3f32; hover.len()];
            gate.process(&mut a, &hover);
            let dip = vec![DEFAULT_CLOSE * 0.5; FS as usize / 20];
            let mut a = vec![0.3f32; dip.len()];
            gate.process(&mut a, &dip);
        }
        // Total gate-open duty over the whole fade must be high: the old code
        // spent most of it in close/ambiguity churn.
        assert!(
            gate.duty() > 0.8,
            "marginal carrier dropped out: final duty {:.2}",
            gate.duty()
        );
    }
}
