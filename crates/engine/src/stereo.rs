//! Broadcast FM audio from the discriminator's multiplex (MPX): the mono
//! sum, and the stereo difference on its 38 kHz subcarrier when the station
//! sends one, blended toward mono as the signal weakens.
//!
//! The multiplex, per ITU-R BS.450, at 75 kHz peak deviation:
//!
//! ```text
//!   0.9 · [ (L+R)/2  +  (L−R)/2 · sin 2ωt ]  +  0.1 · sin ωt     (ω = 2π·19 kHz)
//! ```
//!
//! The sum M = (L+R)/2 is the audio band below 15 kHz; it is what a mono
//! receiver plays, and this path computes it exactly as the mono receiver
//! always has. The difference S = (L−R)/2 comes back by multiplying the
//! multiplex by 2·sin 2ωt, the subcarrier regenerated at twice the phase of
//! a PLL on the pilot, and low-passing what lands in the audio band. Then
//! L = M + S and R = M − S.
//!
//! The output is AC-coupled (the discriminator's DC is the carrier offset)
//! and full deviation plays at half scale, [`WFM_AUDIO_SCALE`], so the
//! peaks a hot station sends past 75 kHz are heard rather than clamped.
//!
//! De-emphasis and the audio low-pass are linear, so running them on M and S
//! and matrixing afterward is the same filter as running them on L and R:
//! each channel is de-emphasized after the matrix, never the composite
//! (which would take the subcarrier down with it). The two branches share one
//! filter design, so they stay matched and the separation is not spent on a
//! mismatch.
//!
//! The difference channel is where the noise is: an FM discriminator's noise
//! rises with frequency, and S comes from 23–53 kHz, so stereo is about 22 dB
//! noisier than mono from the same signal. The decoder measures that noise
//! two ways and blends S toward zero as the S/N falls:
//!
//! - beside the pilot, in quadrature to it, scaled by the discriminator's f²
//!   noise shape to what the stereo channels carry;
//! - in the subcarrier's own quadrature (cos 2ωt), which carries the stereo
//!   band's noise and multipath but no program.
//!
//! Each can only read high: a station's audio processing leaves products
//! near 19 kHz, and loud stereo program leaks into the quadrature when the
//! station's pilot and subcarrier are a degree or two out of phase. On the
//! air the two differ by 2–8 dB, never in the same direction for long, so
//! the lower noise of the two is the reading. The blend falls quickly and recovers slowly, as a car radio's does,
//! so a fade or a burst of multipath goes to mono at once and a weak station
//! does not flutter between the two.

use crate::biquad::{Biquad, Cascade};
use crate::pilot::{PILOT_HZ, PilotPll};
use crate::rds::{Bq, butter4};
use scannerd_dsp::OnePole;
use serde::Serialize;

/// Full deviation (75 kHz) plays at this fraction of full scale. Program
/// peaks routinely pass 75 kHz: stations run hot, the 15 kHz low-pass rings
/// on a transient, and the pilot notch adds its own overshoot. At unity they
/// met the clamp, and 0.5% of a loud station's samples were flattened there
/// (most 10 ms windows of program touched it). Half scale leaves 6 dB for
/// them and still plays processed broadcast audio at about -16 dBFS RMS.
pub const WFM_AUDIO_SCALE: f32 = 0.5;

/// The audio's DC tracker, as a time constant: a first-order high-pass near
/// 3 Hz. An FM discriminator's DC is the carrier offset, not program, and a
/// few kilohertz of crystal or station error is a few percent of full scale
/// that would otherwise come out of the headroom.
pub const WFM_DC_TAU_S: f32 = 0.05;

/// The broadcast audio band: a 12th-order Butterworth at 15 kHz and a notch
/// on the 19 kHz stereo pilot, run at the discriminator's rate.
pub fn wfm_audio_filter(fs: f64) -> Cascade {
    let fs = fs as f32;
    Cascade::butterworth_lowpass(15_000.0, fs, 6).then(Biquad::notch(19_000.0, fs, 4.0))
}

/// Full separation at or above this estimated stereo S/N (dB, against full
/// deviation, de-emphasized, 15 kHz); mono at or below `BLEND_MONO_DB`.
pub const BLEND_FULL_DB: f32 = 44.0;
pub const BLEND_MONO_DB: f32 = 26.0;
/// The blend falls toward mono with this time constant, and climbs back at
/// no more than the whole way in `RECOVER_S`: a slew rather than a decay, so
/// a strong station does reach full separation rather than only approach it.
const ATTACK_S: f64 = 0.03;
const RECOVER_S: f64 = 2.0;
/// Pilot injection that counts as a real pilot, as deviation. The standard
/// is 8–10% of 75 kHz (6–7.5 kHz); this allows for a station run hot or
/// light, and refuses a stray 19 kHz tone in a mono station's audio, which
/// would be either faint or far louder than a pilot.
const PILOT_MIN_HZ: f64 = 3_500.0;
const PILOT_MAX_HZ: f64 = 12_000.0;
/// The noise probe's bandwidth either side of the pilot, and how long its
/// readings are averaged.
const PROBE_HZ: f64 = 400.0;
const PROBE_SMOOTH_S: f64 = 0.05;
/// How long the subcarrier's skew is averaged over. A station's subcarrier
/// sits at a fixed phase to its pilot, so this can be slow; slower than the
/// noise reading, so a loud passage does not swing it.
const SKEW_SMOOTH_S: f64 = 1.0;
/// The most skew believed: a few degrees is common, more is a measurement
/// thrown by a passage with no stereo in it.
const SKEW_MAX: f64 = 0.2;
/// The blend is reconsidered this often (samples at the MPX rate).
const BLEND_EVERY: usize = 64;

/// What the stereo decoder measured, for the status and the audio frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct StereoStatus {
    /// A pilot is locked and its level is a pilot's.
    pub pilot: bool,
    /// How much of the difference signal is in the output, 0 (mono) to 1.
    pub blend: f32,
    /// The S/N the stereo channels would have, from the noise beside the
    /// pilot; only while a pilot is locked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snr_db: Option<f32>,
    /// The pilot's injection, as deviation in Hz; only while locked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pilot_dev_hz: Option<f32>,
    /// The channel's multipath, as its envelope's spread in percent of the
    /// mean, and whether the equalizer is undoing it; filled by the
    /// receiver, which runs the equalizer ahead of this decoder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multipath_pct: Option<f32>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub equalizer: bool,
}

/// The WFM receiver's audio chain, from discriminator hertz to mono and
/// (when there is a pilot) interleaved stereo at the decimated rate.
pub struct WfmAudio {
    fs: f64,
    deviation_hz: f32,
    tau_s: f32,
    /// The mono sum: de-emphasis, then the audio band, then the carrier
    /// offset tracked out of it.
    deemph: OnePole,
    lp: Cascade,
    dc: OnePole,
    /// The difference, after the subcarrier is taken off.
    deemph_s: OnePole,
    lp_s: Cascade,
    /// The subcarrier's quadrature, through the same filters as S, and its
    /// power averaged like the probe's.
    deemph_q: OnePole,
    lp_q: Cascade,
    sq_pow: f64,
    /// The station's subcarrier skew against the pilot-derived reference,
    /// as the regression of the quadrature on the in-phase difference: the
    /// cross power and the difference's power, slowly averaged, and their
    /// ratio (tan of the skew).
    cross: f64,
    s_pow: f64,
    skew: f64,
    skew_k: f64,
    /// Each reading, for `USDR_STEREO_TRACE`.
    trace: Option<(usize, f64)>,
    decim: usize,
    phase: usize,
    pilot: PilotPll,
    /// The pilot mixed to DC: in phase (its level) and in quadrature (the
    /// noise beside it), each low-passed, then averaged.
    probe_i: [Bq; 2],
    probe_q: [Bq; 2],
    level: f64,
    q_mean: f64,
    q_var: f64,
    smooth_k: f64,
    /// Noise in the stereo channels per unit of noise density at 19 kHz,
    /// and the mono branch's share of it against the difference branch's.
    noise_k: f64,
    mono_share: f64,
    blend: f64,
    attack_k: f64,
    recover_step: f64,
    until_blend: usize,
    /// The difference branch runs (and stereo is delivered) this block.
    active: bool,
    status: StereoStatus,
}

impl WfmAudio {
    /// `fs` is the discriminator's rate; `decim` the integer step down to
    /// the audio rate; `deviation_hz` full scale; `tau_s` the de-emphasis
    /// time constant (75 µs in the Americas, 50 µs elsewhere).
    pub fn new(fs: f64, decim: usize, deviation_hz: f32, tau_s: f32) -> Self {
        let dt = BLEND_EVERY as f64 / fs;
        Self {
            fs,
            deviation_hz,
            tau_s,
            deemph: OnePole::new(tau_s * fs as f32),
            lp: wfm_audio_filter(fs),
            dc: OnePole::new(WFM_DC_TAU_S * fs as f32),
            deemph_s: OnePole::new(tau_s * fs as f32),
            lp_s: wfm_audio_filter(fs),
            deemph_q: OnePole::new(tau_s * fs as f32),
            lp_q: wfm_audio_filter(fs),
            sq_pow: 0.0,
            cross: 0.0,
            s_pow: 0.0,
            skew: 0.0,
            skew_k: 1.0 / (SKEW_SMOOTH_S * fs),
            trace: std::env::var_os("USDR_STEREO_TRACE").map(|_| (0, 0.0)),
            decim: decim.max(1),
            phase: 0,
            pilot: PilotPll::new(fs),
            probe_i: butter4(PROBE_HZ, fs),
            probe_q: butter4(PROBE_HZ, fs),
            level: 0.0,
            q_mean: 0.0,
            q_var: 0.0,
            smooth_k: 1.0 / (PROBE_SMOOTH_S * fs),
            noise_k: stereo_noise_factor(tau_s as f64),
            mono_share: {
                let (m, s) = noise_parts(tau_s as f64);
                m / s
            },
            blend: 0.0,
            attack_k: 1.0 - (-dt / ATTACK_S).exp(),
            recover_step: dt / RECOVER_S,
            until_blend: BLEND_EVERY,
            active: false,
            status: StereoStatus::default(),
        }
    }

    pub fn decim(&self) -> usize {
        self.decim
    }

    pub fn status(&self) -> StereoStatus {
        self.status
    }

    /// Back to nothing heard, after a break in the samples: the pilot's
    /// phase does not survive a hole in the timeline.
    pub fn reset(&mut self) {
        *self = Self::new(self.fs, self.decim, self.deviation_hz, self.tau_s);
    }

    /// One block of discriminator output, in Hz. Mono audio is appended to
    /// `mono` (always); interleaved L, R to `stereo` for the whole block when
    /// the decoder is delivering stereo, so `stereo` either grows by twice
    /// what `mono` did or not at all.
    pub fn process(&mut self, disc_hz: &[f32], mono: &mut Vec<f32>, stereo: &mut Vec<f32>) {
        let active = self.status.pilot || self.blend > 1e-3;
        if self.active && !active {
            // Back to mono: the difference branch rests, and starts again
            // from silence, under a blend that rises from zero.
            self.deemph_s.reset();
            self.lp_s.reset();
            self.deemph_q.reset();
            self.lp_q.reset();
            self.cross = 0.0;
            self.s_pow = 0.0;
            self.blend = 0.0;
            self.status.blend = 0.0;
        }
        if active && !self.active {
            // The quadrature's average starts from the probe's reading, so
            // it cannot promise a quiet band before it has heard one.
            self.sq_pow = self.pilot_side_noise();
        }
        self.active = active;
        for &hz in disc_hz {
            // Divided, not multiplied by a reciprocal: the mono numbers stay
            // exactly what the mono receiver produced.
            let x = hz / self.deviation_hz;
            let xd = x as f64;
            let (c, s) = self.pilot.step(xd);
            // The probe: level in phase, noise in quadrature.
            let mut i = xd * c;
            let mut q = xd * s;
            for f in &mut self.probe_i {
                i = f.process(i);
            }
            for f in &mut self.probe_q {
                q = f.process(q);
            }
            self.level += (i - self.level) * self.smooth_k;
            self.q_mean += (q - self.q_mean) * self.smooth_k;
            let d = q - self.q_mean;
            self.q_var += (d * d - self.q_var) * self.smooth_k;

            let m = self.lp.process(self.deemph.process(x));
            let m = m - self.dc.process(m);
            // The PLL holds the pilot at cos θ, so the subcarrier, which
            // crosses zero rising with the pilot (sin ωt = cos(θ), sin 2ωt),
            // is −sin 2θ = −2 sin θ cos θ; times two to bring S back at its
            // own level.
            let sd = if active {
                let quad = (c * c - s * s) as f32;
                let sq = self.lp_q.process(self.deemph_q.process(2.0 * x * quad)) as f64;
                let sub = -2.0 * s * c;
                let si = self.lp_s.process(self.deemph_s.process(2.0 * x * sub as f32)) as f64;
                // A subcarrier a degree or two off the pilot puts sin(skew)
                // of the program into the quadrature, which then reads as
                // noise and holds a strong station short of full stereo.
                // The leak is coherent with S, so its share is measured and
                // taken out: what is left is the noise and the multipath.
                self.cross += (sq * si - self.cross) * self.skew_k;
                self.s_pow += (si * si - self.s_pow) * self.skew_k;
                // Only while there is stereo to measure it on: in a mono
                // passage both are noise and the ratio is no skew at all.
                if self.s_pow > 1e-9 && self.s_pow > 4.0 * self.sq_pow {
                    self.skew = (self.cross / self.s_pow).clamp(-SKEW_MAX, SKEW_MAX);
                }
                let resid = sq - self.skew * si;
                self.sq_pow += (resid * resid - self.sq_pow) * self.smooth_k;
                // And S on the station's own phase: the two readings rotated
                // by the skew, so none of S is left in the quadrature.
                ((si + self.skew * sq) / (1.0 + self.skew * self.skew).sqrt()) as f32
            } else {
                0.0
            };
            self.phase += 1;
            if self.phase >= self.decim {
                self.phase = 0;
                mono.push((m * WFM_AUDIO_SCALE).clamp(-1.0, 1.0));
                if active {
                    let b = self.blend as f32 * sd;
                    stereo.push(((m + b) * WFM_AUDIO_SCALE).clamp(-1.0, 1.0));
                    stereo.push(((m - b) * WFM_AUDIO_SCALE).clamp(-1.0, 1.0));
                }
            }
            self.until_blend -= 1;
            if self.until_blend == 0 {
                self.until_blend = BLEND_EVERY;
                self.update_blend();
            }
        }
    }

    fn update_blend(&mut self) {
        let dev_hz = 2.0 * self.level * self.deviation_hz as f64;
        let pilot = self.pilot.locked && (PILOT_MIN_HZ..=PILOT_MAX_HZ).contains(&dev_hz);
        let snr_db = pilot.then(|| {
            // The lower of the two noise readings; the quadrature carries
            // S's noise, and M's adds under a hundredth of it.
            let quad = if self.active { self.sq_pow * (1.0 + self.mono_share) } else { f64::MAX };
            let noise = self.pilot_side_noise().min(quad).max(1e-15);
            10.0 * (0.5 / noise).log10()
        });
        let target = snr_db.map_or(0.0, |db| {
            ((db - BLEND_MONO_DB as f64) / (BLEND_FULL_DB - BLEND_MONO_DB) as f64).clamp(0.0, 1.0)
        });
        if target < self.blend {
            self.blend += (target - self.blend) * self.attack_k;
        } else {
            self.blend = (self.blend + self.recover_step).min(target);
        }
        if let Some((n, _)) = &mut self.trace {
            *n += BLEND_EVERY;
            if *n as f64 >= 2.0 * self.fs {
                *n = 0;
                let db = |noise: f64| 10.0 * (0.5 / noise.max(1e-15)).log10();
                eprintln!(
                    "STEREO pilot {pilot} dev {dev_hz:.0} Hz · S/N beside the pilot {:.1} dB · in the quadrature {:.1} dB · skew {:.2}° · blend {:.2}",
                    db(self.pilot_side_noise()),
                    db(self.sq_pow * (1.0 + self.mono_share)),
                    self.skew.atan().to_degrees(),
                    self.blend
                );
            }
        }
        self.status = StereoStatus {
            pilot,
            blend: self.blend as f32,
            snr_db: snr_db.map(|d| d as f32),
            pilot_dev_hz: pilot.then_some(dev_hz as f32),
            ..Default::default()
        };
    }

    /// The noise one stereo channel carries, from the probe beside the
    /// pilot: the noise density at 19 kHz (each side of the pilot folds onto
    /// DC at a quarter of its density, through the probe's equivalent noise
    /// bandwidth), scaled by the f² shape.
    fn pilot_side_noise(&self) -> f64 {
        let n19 = 2.0 * self.q_var / (PROBE_HZ * BUTTER4_ENBW);
        n19 * self.noise_k
    }

}

/// A fourth-order Butterworth's equivalent noise bandwidth, in units of its
/// cutoff: (π/8) / sin(π/8).
const BUTTER4_ENBW: f64 = 1.026_172;

/// The noise power in one stereo channel (L = M + S, so the two branches'
/// noise adds) per unit of noise density at 19 kHz, for a discriminator
/// whose noise density rises as f²: the mono band's own, plus what the
/// 2·sin 2ωt mixer brings down from either side of 38 kHz, both through the
/// de-emphasis and the audio low-pass.
fn stereo_noise_factor(tau_s: f64) -> f64 {
    let (m, s) = noise_parts(tau_s);
    m + s
}

/// The mono branch's and the difference branch's parts of that.
fn noise_parts(tau_s: f64) -> (f64, f64) {
    let sub = 2.0 * PILOT_HZ;
    let df = 10.0;
    let (mut m, mut s) = (0.0, 0.0);
    let mut f = df / 2.0;
    while f < 30_000.0 {
        let wt = std::f64::consts::TAU * f * tau_s;
        let h = 1.0 / (1.0 + wt * wt) / (1.0 + (f / 15_000.0).powi(24)) * df;
        m += f * f * h;
        s += ((sub - f).powi(2) + (sub + f).powi(2)) * h;
        f += df;
    }
    let n = PILOT_HZ * PILOT_HZ;
    (m / n, s / n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nbfm::NbfmDemod;
    use num_complex::Complex32;
    use std::f64::consts::TAU;

    const FS: f64 = 256_000.0;
    const DECIM: usize = 5;
    const DEV: f32 = 75_000.0;
    const TAU_S: f32 = 75e-6;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
        fn gauss(&mut self) -> f64 {
            let u = self.next().max(1e-300);
            let v = self.next();
            (-2.0 * u.ln()).sqrt() * (TAU * v).cos()
        }
    }

    /// A stereo multiplex, normalized to full deviation, with `l` and `r`
    /// as functions of time.
    fn mpx(secs: f64, pilot: bool, l: impl Fn(f64) -> f64, r: impl Fn(f64) -> f64) -> Vec<f64> {
        let n = (secs * FS) as usize;
        (0..n)
            .map(|k| {
                let t = k as f64 / FS;
                let w = TAU * PILOT_HZ * t;
                let (lv, rv) = (l(t), r(t));
                let sub = if pilot { (lv - rv) / 2.0 * (2.0 * w).sin() } else { 0.0 };
                0.9 * ((lv + rv) / 2.0 + sub) + if pilot { 0.1 * w.sin() } else { 0.0 }
            })
            .collect()
    }

    /// The multiplex frequency-modulated onto I/Q with complex noise of
    /// standard deviation `sigma` per component, through the receiver's own
    /// discriminator: the noise comes out shaped as it is on the air.
    fn on_air(mpx: &[f64], sigma: f64, seed: u64) -> Vec<f32> {
        let mut rng = Rng(seed);
        let mut ph = 0.0f64;
        let iq: Vec<Complex32> = mpx
            .iter()
            .map(|&x| {
                ph = (ph + TAU * DEV as f64 * x / FS).rem_euclid(TAU);
                Complex32::new((ph.cos() + sigma * rng.gauss()) as f32, (ph.sin() + sigma * rng.gauss()) as f32)
            })
            .collect();
        let mut fm = NbfmDemod::with_deviation(FS, DEV);
        let mut disc = Vec::new();
        fm.discriminator_hz(&iq, &mut disc);
        disc
    }

    /// Run the chain in blocks the size the server uses, returning mono,
    /// stereo (only from blocks that delivered it, else empty) and the last
    /// status.
    fn run(dec: &mut WfmAudio, disc: &[f32]) -> (Vec<f32>, Vec<f32>, StereoStatus) {
        let (mut mono, mut stereo) = (Vec::new(), Vec::new());
        let mut st = Vec::new();
        for block in disc.chunks(2_560) {
            let before = mono.len();
            st.clear();
            dec.process(block, &mut mono, &mut st);
            if st.is_empty() {
                stereo.clear();
            } else {
                assert_eq!(st.len(), 2 * (mono.len() - before));
                stereo.extend_from_slice(&st);
            }
        }
        (mono, stereo, dec.status())
    }

    /// Amplitude of a `hz` tone in `x` at the audio rate.
    fn tone(x: &[f32], hz: f64) -> f64 {
        let fs = FS / DECIM as f64;
        let (mut i, mut q) = (0.0, 0.0);
        for (k, &v) in x.iter().enumerate() {
            let w = TAU * hz * k as f64 / fs;
            i += v as f64 * w.cos();
            q += v as f64 * w.sin();
        }
        2.0 * i.hypot(q) / x.len() as f64
    }

    /// Channel `ch` of the last `secs` of interleaved stereo.
    fn channel(stereo: &[f32], ch: usize, secs: f64) -> Vec<f32> {
        let frames = ((secs * FS / DECIM as f64) as usize).min(stereo.len() / 2);
        stereo[stereo.len() - 2 * frames..].iter().skip(ch).step_by(2).copied().collect()
    }

    /// Complex noise per component that gives a stereo S/N of `db`, by the
    /// same f² model the decoder uses.
    fn sigma_for(db: f64) -> f64 {
        // Frequency noise from phase noise σ²/fs per Hz each side: a one-sided
        // density of 2σ²/fs · f², in units of full deviation.
        let n19_per_sigma2 = 2.0 / FS * (PILOT_HZ / DEV as f64).powi(2);
        let noise = 0.5 / 10f64.powf(db / 10.0);
        (noise / stereo_noise_factor(TAU_S as f64) / n19_per_sigma2).sqrt()
    }

    #[test]
    fn a_left_only_tone_stays_left() {
        let x = mpx(4.0, true, |t| 0.5 * (TAU * 1_000.0 * t).sin(), |_| 0.0);
        let disc = on_air(&x, sigma_for(60.0), 1);
        let mut dec = WfmAudio::new(FS, DECIM, DEV, TAU_S);
        let (_, stereo, st) = run(&mut dec, &disc);
        assert!(st.pilot && st.blend == 1.0, "{st:?}");
        let (l, r) = (channel(&stereo, 0, 1.0), channel(&stereo, 1, 1.0));
        let (al, ar) = (tone(&l, 1_000.0), tone(&r, 1_000.0));
        let sep = 20.0 * (al / ar).log10();
        println!("separation {sep:.1} dB (L {al:.4}, R {ar:.6}), {st:?}");
        assert!(sep >= 30.0, "separation {sep:.1} dB");
        // And the other way round.
        let x = mpx(4.0, true, |_| 0.0, |t| 0.5 * (TAU * 3_000.0 * t).sin());
        let mut dec = WfmAudio::new(FS, DECIM, DEV, TAU_S);
        let (_, stereo, _) = run(&mut dec, &on_air(&x, sigma_for(60.0), 2));
        let (l, r) = (channel(&stereo, 0, 1.0), channel(&stereo, 1, 1.0));
        let sep = 20.0 * (tone(&r, 3_000.0) / tone(&l, 3_000.0)).log10();
        println!("separation R at 3 kHz {sep:.1} dB");
        assert!(sep >= 30.0, "separation {sep:.1} dB");
    }

    #[test]
    fn no_pilot_stays_mono() {
        let x = mpx(1.5, false, |t| 0.5 * (TAU * 1_000.0 * t).sin(), |_| 0.0);
        let disc = on_air(&x, sigma_for(50.0), 3);
        let mut dec = WfmAudio::new(FS, DECIM, DEV, TAU_S);
        let (mono, stereo, st) = run(&mut dec, &disc);
        assert!(!st.pilot && st.blend == 0.0 && st.snr_db.is_none(), "{st:?}");
        assert!(stereo.is_empty());
        // The mono receiver's chain, sample for sample.
        let mut deemph = OnePole::new(TAU_S * FS as f32);
        let mut lp = wfm_audio_filter(FS);
        let mut dc = OnePole::new(WFM_DC_TAU_S * FS as f32);
        let mut want = Vec::new();
        let mut phase = 0;
        for &hz in &disc {
            let v = lp.process(deemph.process(hz / DEV));
            let v = v - dc.process(v);
            phase += 1;
            if phase >= DECIM {
                want.push((v * WFM_AUDIO_SCALE).clamp(-1.0, 1.0));
                phase = 0;
            }
        }
        assert_eq!(mono, want);
    }

    #[test]
    fn a_hot_station_off_frequency_keeps_its_headroom() {
        // Mono bass at 110% of full deviation, on a carrier 6 kHz off. At
        // unity scale this sat on the clamp; now it has room, and the
        // offset is tracked out rather than spent from the headroom.
        let off = 6_000.0 / DEV as f64;
        let n = (1.5 * FS) as usize;
        let x: Vec<f64> = (0..n).map(|k| off + 1.1 * (TAU * 200.0 * k as f64 / FS).sin()).collect();
        let mut dec = WfmAudio::new(FS, DECIM, DEV, TAU_S);
        let (mono, _, _) = run(&mut dec, &on_air(&x, sigma_for(50.0), 8));
        let tail = &mono[mono.len() / 2..];
        let peak = tail.iter().fold(0.0f32, |a, &v| a.max(v.abs()));
        let mean = tail.iter().map(|&v| v as f64).sum::<f64>() / tail.len() as f64;
        let amp = tone(tail, 200.0);
        assert!(peak < 0.75, "peak {peak}");
        assert!(mean.abs() < 0.005, "DC {mean}");
        assert!((amp / (1.1 * WFM_AUDIO_SCALE as f64) - 1.0).abs() < 0.03, "200 Hz at {amp}");
    }

    #[test]
    fn a_loud_19_khz_tone_is_not_a_pilot() {
        // A mono station with a 19 kHz whistle at three times a pilot's level.
        let n = (2.0 * FS) as usize;
        let x: Vec<f64> = (0..n).map(|k| 0.3 * (TAU * PILOT_HZ * k as f64 / FS).sin()).collect();
        let mut dec = WfmAudio::new(FS, DECIM, DEV, TAU_S);
        let (_, stereo, st) = run(&mut dec, &on_air(&x, sigma_for(60.0), 4));
        assert!(!st.pilot && stereo.is_empty(), "{st:?}");
    }

    #[test]
    fn the_noise_estimate_matches_the_noise() {
        // A silent program at 50 dB: full stereo, and the noise in L should
        // be what the probe beside the pilot predicts.
        let x = mpx(4.0, true, |_| 0.0, |_| 0.0);
        let mut dec = WfmAudio::new(FS, DECIM, DEV, TAU_S);
        let (_, stereo, st) = run(&mut dec, &on_air(&x, sigma_for(50.0), 5));
        let l = channel(&stereo, 0, 1.5);
        let p = l.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / l.len() as f64;
        // Against a full-deviation sine, as played: at the audio scale.
        let full = 0.5 * (WFM_AUDIO_SCALE as f64).powi(2);
        let measured = 10.0 * (full / p).log10();
        let est = st.snr_db.unwrap() as f64;
        println!("S/N measured {measured:.1} dB, estimated {est:.1} dB, {st:?}");
        assert!((measured - est).abs() < 2.0, "measured {measured:.1}, estimated {est:.1}");
        assert!((est - 50.0).abs() < 2.0, "estimated {est:.1}");
        assert_eq!(st.blend, 1.0);
    }

    #[test]
    fn a_skewed_subcarrier_and_loud_stereo_do_not_cost_the_blend() {
        // Loud, wide stereo on a subcarrier 3° off the pilot: the program
        // leaks into the subcarrier's quadrature, which the probe beside
        // the pilot does not see.
        let n = (4.0 * FS) as usize;
        let skew = 3f64.to_radians();
        let x: Vec<f64> = (0..n)
            .map(|k| {
                let t = k as f64 / FS;
                let w = TAU * PILOT_HZ * t;
                let (l, r) = (0.7 * (TAU * 700.0 * t).sin(), 0.7 * (TAU * 2_300.0 * t).sin());
                0.9 * ((l + r) / 2.0 + (l - r) / 2.0 * (2.0 * w + skew).sin()) + 0.1 * w.sin()
            })
            .collect();
        let mut dec = WfmAudio::new(FS, DECIM, DEV, TAU_S);
        let (_, stereo, st) = run(&mut dec, &on_air(&x, sigma_for(50.0), 10));
        println!("{st:?}");
        assert_eq!(st.blend, 1.0, "{st:?}");
        assert!((st.snr_db.unwrap() - 50.0).abs() < 3.0, "{st:?}");
        let (l, r) = (channel(&stereo, 0, 1.0), channel(&stereo, 1, 1.0));
        let sep = 20.0 * (tone(&l, 700.0) / tone(&r, 700.0)).log10();
        assert!(sep >= 30.0, "separation {sep:.1} dB");
    }

    /// A processed commercial station: loud wide stereo on a subcarrier 2°
    /// off the pilot, and the composite clipper's products spread around
    /// 19 kHz. Both readings used to see program, not noise, and held the
    /// blend short of full stereo on a station with 55 dB of real S/N.
    #[test]
    fn a_processed_station_with_a_skewed_subcarrier_gets_full_stereo() {
        let n = (5.0 * FS) as usize;
        let skew = 2f64.to_radians();
        let mut rng = Rng(77);
        // Clipper products: tones scattered ±300 Hz about the pilot, each
        // with its own phase, none on it.
        let spurs: Vec<(f64, f64)> = (0..24).map(|i| (PILOT_HZ + (i as f64 - 11.5) * 25.0, rng.next() * TAU)).collect();
        let x: Vec<f64> = (0..n)
            .map(|k| {
                let t = k as f64 / FS;
                let w = TAU * PILOT_HZ * t;
                let (l, r) = (0.7 * (TAU * 700.0 * t).sin(), 0.7 * (TAU * 2_300.0 * t).sin());
                let clip: f64 = spurs.iter().map(|(f, ph)| (TAU * f * t + ph).sin()).sum::<f64>() * 0.0004;
                0.9 * ((l + r) / 2.0 + (l - r) / 2.0 * (2.0 * w + skew).sin()) + 0.1 * w.sin() + clip
            })
            .collect();
        let mut dec = WfmAudio::new(FS, DECIM, DEV, TAU_S);
        let (_, stereo, st) = run(&mut dec, &on_air(&x, sigma_for(55.0), 12));
        println!("{st:?} skew {:.2}° beside the pilot {:.1} dB", dec.skew.atan().to_degrees(), 10.0 * (0.5 / dec.pilot_side_noise()).log10());
        // The skew measured is the station's plus the pilot loop's own small
        // error, which is what the decoder has to undo.
        assert!((1.5..3.5).contains(&dec.skew.atan().to_degrees().abs()), "skew {:.2}°", dec.skew.atan().to_degrees());
        assert!(10.0 * (0.5 / dec.pilot_side_noise()).log10() < 44.0, "the clipper products should spoil the probe in this test");
        assert_eq!(st.blend, 1.0, "{st:?}");
        // The clipper's products that reach the stereo band count; the
        // program that leaked into the quadrature no longer does.
        assert!(st.snr_db.unwrap() > BLEND_FULL_DB, "{st:?}");
        let (l, r) = (channel(&stereo, 0, 1.0), channel(&stereo, 1, 1.0));
        let sep = 20.0 * (tone(&l, 700.0) / tone(&r, 700.0)).log10();
        println!("separation {sep:.1} dB");
        assert!(sep >= 40.0, "separation {sep:.1} dB");
    }

    #[test]
    fn a_noisy_station_blends_to_mono() {
        let x = mpx(4.0, true, |t| 0.5 * (TAU * 1_000.0 * t).sin(), |_| 0.0);
        let mut dec = WfmAudio::new(FS, DECIM, DEV, TAU_S);
        let (_, stereo, st) = run(&mut dec, &on_air(&x, sigma_for(18.0), 6));
        println!("{st:?}");
        assert!(st.blend < 0.02, "{st:?}");
        assert!(st.snr_db.is_none_or(|d| d < BLEND_MONO_DB), "{st:?}");
        // Whatever is still delivered as stereo is all but mono.
        if !stereo.is_empty() {
            let (l, r) = (channel(&stereo, 0, 1.0), channel(&stereo, 1, 1.0));
            let diff = l.iter().zip(&r).map(|(a, b)| ((a - b) as f64).powi(2)).sum::<f64>();
            let sum = l.iter().map(|&a| (a as f64).powi(2)).sum::<f64>();
            assert!(diff < sum * 1e-3, "L−R {diff} against L {sum}");
        }
    }

    #[test]
    fn a_fade_goes_to_mono_fast_and_comes_back_slowly() {
        let mut dec = WfmAudio::new(FS, DECIM, DEV, TAU_S);
        let strong = on_air(&mpx(3.0, true, |_| 0.0, |_| 0.0), sigma_for(55.0), 7);
        let (_, _, st) = run(&mut dec, &strong);
        assert!(st.blend > 0.9, "{st:?}");
        // 150 ms of deep fade.
        let weak = on_air(&mpx(0.15, true, |_| 0.0, |_| 0.0), sigma_for(12.0), 8);
        let (_, _, st) = run(&mut dec, &weak);
        assert!(st.blend < 0.2, "fell to {st:?}");
        // Strong again: half a second later still well short of full.
        let back = on_air(&mpx(0.5, true, |_| 0.0, |_| 0.0), sigma_for(55.0), 9);
        let (_, _, st) = run(&mut dec, &back);
        assert!(st.blend < 0.6, "recovered to {st:?} in 0.5 s");
    }
}
