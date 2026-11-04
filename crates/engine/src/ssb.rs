//! Single-sideband (USB/LSB) voice demodulation for HF.
//!
//! SSB transmits one sideband of the voice and no carrier, so there is
//! nothing to rectify: the audio is the baseband signal itself, translated
//! down to 0 Hz by the tuning. What the demodulator has to do is pick ONE
//! side of 0 Hz and throw the other away — the opposite sideband is a
//! different station (or the noise on top of it), and it lands on the same
//! audio frequencies once the real part is taken.
//!
//! This uses the Weaver method, which needs no Hilbert transformer:
//!
//! 1. mix the wanted 300–2800 Hz band down by its center (1550 Hz) so it
//!    sits symmetrically on ±1250 Hz;
//! 2. low-pass I and Q to ±1250 Hz — the opposite sideband now lies at
//!    1850 Hz and beyond, on the far side of a sharp skirt;
//! 3. mix back up by the same 1550 Hz and take the real part.
//!
//! Because steps 1 and 3 use the *same* oscillator, the chain is exactly a
//! linear time-invariant band-pass: the oscillator's absolute phase, and any
//! rounding walk in it, cancels between the two mixers. Nothing drifts.
//!
//! The low-pass is an 8th-order Chebyshev (0.5 dB ripple) built from four
//! existing RBJ biquad sections. A linear-phase FIR with the same 600 Hz
//! transition would need ~235 taps per rail; four biquads per rail are about
//! a tenth of the arithmetic, and speech does not care about the non-linear
//! phase at the band edges.
//!
//! There is no carrier to level against, so the AGC tracks the speech
//! envelope's peaks instead: fast attack, a hang that holds the gain across
//! the gaps between words, then a slow decay. A gain ceiling keeps the hiss
//! between overs from being pulled up to full scale. The ceiling follows the
//! band's own noise floor: a quiet band lets a weak, clean signal have all
//! the gain it needs, while a noisy one is held where its hiss sits well
//! under the speech.

use crate::biquad::{Biquad, Cascade};
use num_complex::Complex32;
use scannerd_dsp::OnePole;
use std::f64::consts::PI;

/// Low edge of the SSB voice passband, in audio Hz (USB: +300 Hz on the
/// baseband, LSB: −300 Hz). Below this sits the suppressed carrier.
pub const SSB_LOW_HZ: f32 = 300.0;
/// High edge of the SSB voice passband, in audio Hz.
pub const SSB_HIGH_HZ: f32 = 2_800.0;

/// AGC gain the band's noise can never take away. Whatever arrives below
/// `TARGET_PEAK / 10^(60/20)` (a passband envelope of 6e-4, about −64 dBFS)
/// gets at least this much; noise louder than −64 dBFS is treated like any
/// other signal — muting that is the squelch's job, not the AGC's.
pub const SSB_MAX_GAIN_DB: f32 = 60.0;

/// Gain the AGC may go on to when the band is quiet enough to take it, and
/// never more: a weak signal on a clean band plays at full level, and
/// digital silence (a floor of nothing) cannot ask for unbounded gain.
pub const SSB_QUIET_BAND_GAIN_DB: f32 = 90.0;

/// Between overs, the band's noise floor plays this far under the level
/// speech peaks are steered to. Past [`SSB_MAX_GAIN_DB`] the gain stops
/// where the floor reaches it, so the extra gain is only ever spent on a band
/// quiet enough to afford it.
const HISS_UNDER_DB: f32 = 20.0;

/// The noise floor is the envelope, smoothed over this long, at its quietest:
/// it falls to the envelope within [`FLOOR_FALL_S`] and climbs back at
/// [`FLOOR_RISE_DB_PER_S`]. The gaps between words pull it down to the hiss;
/// a second of unbroken speech lifts it only a few dB.
const FLOOR_SMOOTH_S: f32 = 0.020;
const FLOOR_FALL_S: f32 = 0.050;
const FLOOR_RISE_DB_PER_S: f32 = 3.0;

/// Chebyshev order and ripple. Order 8 at 0.5 dB puts the nearest
/// opposite-sideband frequency (−300 Hz, i.e. 1850 Hz after the shift, 1.48×
/// the corner) ~50 dB down; order 6 manages only ~34 dB.
const LP_ORDER: usize = 8;
const LP_RIPPLE_DB: f64 = 0.5;

/// Input DC blocker time constant: an 8 Hz corner, well clear of the 300 Hz
/// edge. A receiver DC offset (or a carrier sitting at exactly 0 Hz) would
/// otherwise reach the audio as DC, since 0 Hz is only ~32 dB down the
/// Chebyshev skirt.
const DC_TAU_S: f32 = 0.020;

/// Renormalize the rotating phasor this often; f32 rounding grows its
/// magnitude by ~1e-7 per step, so 256 steps keep it within ~3e-5 of unity.
const RENORM_EVERY: u32 = 256;

/// Level the AGC steers envelope peaks to.
const TARGET_PEAK: f32 = 0.6;
/// Peak attack time constant.
const ATTACK_S: f32 = 0.002;
/// Audio is delayed this much behind the envelope detector, so the gain
/// has already come down (by ~92%, 2.5 attack constants) when the onset of
/// an over reaches the output. Without it every first syllable would clip.
const LOOKAHEAD_S: f32 = 0.005;
// The hang (gain held across the gaps in speech) and the recovery rate after
// it come from `AgcSpeed`; medium is 400 ms and 15 dB/s.
/// Output clamp, the backstop for anything the attack misses.
const CLAMP: f32 = 1.5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sideband {
    Upper,
    Lower,
}

pub struct SsbDemod {
    sideband: Sideband,
    /// DC blockers on I and Q (low-passes, subtracted from the input).
    dc_re: OnePole,
    dc_im: OnePole,
    /// Weaver oscillator: the current phasor and the per-sample rotation.
    /// USB rotates at −1550 Hz (down-mix by `phasor`, up-mix by its
    /// conjugate); LSB the other way round.
    phasor: Complex32,
    step: Complex32,
    renorm_count: u32,
    /// The ±1250 Hz low-pass, one per rail.
    lp_re: Cascade,
    lp_im: Cascade,
    /// AGC: tracked envelope peak and the hang countdown, in samples.
    peak: f32,
    /// The tracked peak never falls below the ceiling, which lies between
    /// these two: `peak_floor` at 60 dB of gain, `peak_quiet` at 90.
    peak_floor: f32,
    peak_quiet: f32,
    /// The envelope smoothed for the floor tracker, the floor itself, and
    /// the tracker's per-sample coefficients.
    env_avg: f32,
    noise: f32,
    avg_k: f32,
    fall_k: f32,
    rise: f32,
    hang: u32,
    hang_len: u32,
    attack: f32,
    decay: f32,
    /// Lookahead delay line for the audio, and its write position.
    delay: Vec<f32>,
    delay_pos: usize,
    fs: f64,
    /// The Weaver shift (the passband's center) and its half width, Hz.
    weaver_hz: f64,
}

impl SsbDemod {
    pub fn new(fs: f64, sideband: Sideband) -> Self {
        Self::with_passband(fs, sideband, SSB_LOW_HZ, SSB_HIGH_HZ)
    }

    /// A demodulator passing `low_hz..high_hz` of audio (both positive, on
    /// the sideband's side of the carrier). Moving both edges together is
    /// IF shift; moving them apart or together is the width.
    pub fn with_passband(fs: f64, sideband: Sideband, low_hz: f32, high_hz: f32) -> Self {
        let low = low_hz.max(0.0) as f64;
        let high = (high_hz as f64).max(low + 100.0);
        let weaver_hz = (low + high) / 2.0;
        let half_band = (high - low) / 2.0;
        let fs32 = fs as f32;
        let lookahead = ((LOOKAHEAD_S * fs32).round() as usize).max(1);
        let peak_floor = TARGET_PEAK / 10f32.powf(SSB_MAX_GAIN_DB / 20.0);
        let peak_quiet = TARGET_PEAK / 10f32.powf(SSB_QUIET_BAND_GAIN_DB / 20.0);
        let coef = |s: f32| 1.0 - (-1.0 / (s * fs32).max(1.0)).exp();
        Self {
            sideband,
            dc_re: OnePole::new(DC_TAU_S * fs32),
            dc_im: OnePole::new(DC_TAU_S * fs32),
            phasor: Complex32::new(1.0, 0.0),
            step: Self::rotation(fs, sideband, weaver_hz),
            renorm_count: 0,
            lp_re: chebyshev_lowpass(half_band, fs, LP_ORDER, LP_RIPPLE_DB),
            lp_im: chebyshev_lowpass(half_band, fs, LP_ORDER, LP_RIPPLE_DB),
            peak: peak_floor,
            peak_floor,
            peak_quiet,
            // Starting at the floor that gives exactly the fixed ceiling,
            // the first moments behave as they always have.
            env_avg: peak_floor / hiss_ratio(),
            noise: peak_floor / hiss_ratio(),
            avg_k: coef(FLOOR_SMOOTH_S),
            fall_k: coef(FLOOR_FALL_S),
            rise: 10f32.powf(FLOOR_RISE_DB_PER_S / 20.0 / fs32),
            hang: 0,
            hang_len: (crate::agc::AgcSpeed::Medium.ssb_hang_s() * fs32) as u32,
            attack: 1.0 - (-1.0 / (ATTACK_S * fs32).max(1.0)).exp(),
            // Per-sample multiplier on the tracked peak.
            decay: 10f32.powf(-crate::agc::AgcSpeed::Medium.ssb_decay_db_per_s() / 20.0 / fs32),
            delay: vec![0.0; lookahead],
            delay_pos: 0,
            fs,
            weaver_hz,
        }
    }

    /// Per-sample Weaver rotation for a sideband. Computed in f64 so the
    /// frequency is exact to well under a millihertz at any span rate.
    fn rotation(fs: f64, sideband: Sideband, weaver_hz: f64) -> Complex32 {
        let w = 2.0 * PI * weaver_hz / fs;
        let w = match sideband {
            Sideband::Upper => -w,
            Sideband::Lower => w,
        };
        Complex32::new(w.cos() as f32, w.sin() as f32)
    }

    pub fn sideband(&self) -> Sideband {
        self.sideband
    }

    /// Switch sidebands without rebuilding the filters. The filter state is
    /// kept: the next few milliseconds are a short transient, not a click.
    pub fn set_sideband(&mut self, sideband: Sideband) {
        self.sideband = sideband;
        self.step = Self::rotation(self.fs, sideband, self.weaver_hz);
    }

    /// Demodulate one block: one audio sample per input sample, at the input
    /// rate. Envelope peaks settle near ±0.6; the output never exceeds ±1.5.
    /// The audio lags the input by the filter's group delay plus the 5 ms
    /// AGC lookahead.
    pub fn process(&mut self, input: &[Complex32], out: &mut Vec<f32>) {
        out.clear();
        out.reserve(input.len());
        for &x in input {
            let (audio, env) = self.weaver(x);
            out.push(self.agc(audio, env));
        }
    }

    /// Demodulate one block twice over: `out` through the speech AGC for
    /// the speaker, `linear` straight from the Weaver chain with its level
    /// untouched, for a decoder that weighs signals against each other and
    /// against the noise. A weak FT8 signal beside a strong one is measured
    /// wrongly through an AGC that pumps on the strong one.
    pub fn process_with_linear(&mut self, input: &[Complex32], out: &mut Vec<f32>, linear: &mut Vec<f32>) {
        out.clear();
        out.reserve(input.len());
        linear.clear();
        linear.reserve(input.len());
        for &x in input {
            let (audio, env) = self.weaver(x);
            linear.push(audio);
            out.push(self.agc(audio, env));
        }
    }

    /// The Weaver chain for one sample: returns the (pre-AGC) audio and its
    /// envelope. The envelope is the magnitude of the filtered baseband,
    /// which is exactly the envelope of the real output — smooth, with no
    /// audio-rate ripple for the AGC to chase.
    #[inline]
    fn weaver(&mut self, x: Complex32) -> (f32, f32) {
        let dc = Complex32::new(self.dc_re.process(x.re), self.dc_im.process(x.im));
        let p = self.phasor;
        let d = (x - dc) * p;
        let zr = self.lp_re.process(d.re);
        let zi = self.lp_im.process(d.im);
        // Re{z · conj(p)}: the up-mix, keeping only the real part.
        let audio = zr * p.re + zi * p.im;
        let env = (zr * zr + zi * zi).sqrt();

        self.phasor = p * self.step;
        self.renorm_count += 1;
        if self.renorm_count >= RENORM_EVERY {
            self.renorm_count = 0;
            self.phasor /= self.phasor.norm();
        }
        (audio, env)
    }

    pub fn set_agc(&mut self, speed: crate::agc::AgcSpeed) {
        let fs = self.fs as f32;
        self.hang_len = (speed.ssb_hang_s() * fs) as u32;
        self.decay = 10f32.powf(-speed.ssb_decay_db_per_s() / 20.0 / fs);
    }

    /// The gain ceiling, as the lowest the tracked peak may fall: the
    /// noise floor raised by [`HISS_UNDER_DB`], held between the 60 dB the
    /// band can never take away and the 90 dB a quiet band may have.
    #[inline]
    fn ceiling(&self) -> f32 {
        (self.noise * hiss_ratio()).clamp(self.peak_quiet, self.peak_floor)
    }

    /// The gain the AGC would give a signal too weak to move it, in dB.
    pub fn max_gain_db(&self) -> f32 {
        20.0 * (TARGET_PEAK / self.ceiling()).log10()
    }

    /// Peak-tracking hang AGC with lookahead.
    #[inline]
    fn agc(&mut self, audio: f32, env: f32) -> f32 {
        self.env_avg += (env - self.env_avg) * self.avg_k;
        if self.env_avg < self.noise {
            self.noise += (self.env_avg - self.noise) * self.fall_k;
        } else {
            self.noise *= self.rise;
        }
        if env > self.peak {
            self.peak += (env - self.peak) * self.attack;
            self.hang = self.hang_len;
        } else if self.hang > 0 {
            self.hang -= 1;
        } else {
            self.peak = (self.peak * self.decay).max(self.ceiling());
        }
        let delayed = std::mem::replace(&mut self.delay[self.delay_pos], audio);
        self.delay_pos += 1;
        if self.delay_pos == self.delay.len() {
            self.delay_pos = 0;
        }
        (delayed * (TARGET_PEAK / self.peak)).clamp(-CLAMP, CLAMP)
    }

    pub fn reset(&mut self) {
        self.dc_re.reset();
        self.dc_im.reset();
        self.phasor = Complex32::new(1.0, 0.0);
        self.renorm_count = 0;
        self.lp_re.reset();
        self.lp_im.reset();
        self.peak = self.peak_floor;
        self.env_avg = self.peak_floor / hiss_ratio();
        self.noise = self.env_avg;
        self.hang = 0;
        self.delay.fill(0.0);
        self.delay_pos = 0;
    }

    /// The Weaver chain alone, without AGC, so tests can measure selectivity
    /// that the AGC would otherwise normalize away.
    #[cfg(test)]
    fn process_raw(&mut self, input: &[Complex32], out: &mut Vec<f32>) {
        out.clear();
        for &x in input {
            out.push(self.weaver(x).0);
        }
    }
}

/// [`HISS_UNDER_DB`] as an amplitude ratio.
fn hiss_ratio() -> f32 {
    10f32.powf(HISS_UNDER_DB / 20.0)
}

/// Chebyshev type I low-pass of even `order`, as a cascade of RBJ low-pass
/// sections.
///
/// Each conjugate pole pair of the analog prototype is a second-order
/// low-pass with its own natural frequency `w0` (relative to the corner) and
/// Q. The RBJ section is the bilinear transform of exactly that shape,
/// prewarped at its own frequency, so pre-warping the *corner* instead just
/// means placing each section at `atan(w0 · tan(πfc/fs))`. Each section has
/// unity DC gain, so the passband ripples between 0 and +0.5 dB.
fn chebyshev_lowpass(fc: f64, fs: f64, order: usize, ripple_db: f64) -> Cascade {
    let eps = (10f64.powf(ripple_db / 10.0) - 1.0).sqrt();
    let v = (1.0 / eps).asinh() / order as f64;
    let warp = (PI * fc / fs).tan();
    // Lowest Q first, so the sharp resonant section sees an already
    // band-limited signal.
    let sections = (1..=order / 2)
        .rev()
        .map(|k| {
            let theta = PI * (2 * k - 1) as f64 / (2 * order) as f64;
            let sigma = v.sinh() * theta.sin();
            let omega = v.cosh() * theta.cos();
            let w0 = sigma.hypot(omega);
            let q = w0 / (2.0 * sigma);
            let f_section = (w0 * warp).atan() * fs / PI;
            Biquad::lowpass(f_section as f32, fs as f32, q as f32)
        })
        .collect();
    Cascade::new(sections)
}

#[cfg(test)]
mod tests {

    #[test]
    fn the_linear_output_is_the_chain_before_the_agc() {
        use super::*;
        let fs = 48_000.0;
        let x: Vec<Complex32> = (0..9_600).map(|i| Complex32::from_polar(if i < 4_800 { 0.5 } else { 0.005 }, std::f32::consts::TAU * 1_500.0 * i as f32 / fs as f32)).collect();
        let (mut a, mut b, mut c) = (SsbDemod::new(fs, Sideband::Upper), SsbDemod::new(fs, Sideband::Upper), SsbDemod::new(fs, Sideband::Upper));
        let (mut out, mut lin, mut plain, mut raw) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        a.process_with_linear(&x, &mut out, &mut lin);
        b.process(&x, &mut plain);
        c.process_raw(&x, &mut raw);
        // The speaker's audio is unchanged; the decoder's keeps the 40 dB step
        // the AGC would level away.
        assert_eq!(out, plain);
        assert_eq!(lin, raw);
        let rms = |v: &[f32]| (v.iter().map(|s| s * s).sum::<f32>() / v.len() as f32).sqrt();
        let step = 20.0 * (rms(&lin[2_400..4_800]) / rms(&lin[7_200..])).log10();
        assert!((step - 40.0).abs() < 1.0, "{step}");
    }

    /// Power of `f` in `x`, by Goertzel.
    fn tone_power(x: &[f32], fs: f64, f: f64) -> f64 {
        let w = 2.0 * std::f64::consts::PI * f / fs;
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        for &v in x {
            let s0 = v as f64 + 2.0 * w.cos() * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2
    }

    #[test]
    fn a_shifted_narrow_passband_keeps_what_is_inside_it() {
        let fs = 48_000.0;
        // USB at 1000-2000 Hz: 1500 Hz is in it, 2600 Hz (inside the default
        // 300-2800) is not.
        let mut d = SsbDemod::with_passband(fs, Sideband::Upper, 1_000.0, 2_000.0);
        let n = 48_000;
        let x: Vec<Complex32> = (0..n)
            .map(|i| {
                let t = i as f64 / fs;
                let a = 2.0 * std::f64::consts::PI * 1_500.0 * t;
                let b = 2.0 * std::f64::consts::PI * 2_600.0 * t;
                Complex32::new((0.1 * (a.cos() + b.cos())) as f32, (0.1 * (a.sin() + b.sin())) as f32)
            })
            .collect();
        let mut out = Vec::new();
        d.process(&x, &mut out);
        let tail = &out[n / 2..];
        let inside = tone_power(tail, fs, 1_500.0);
        let outside = tone_power(tail, fs, 2_600.0);
        let db = 10.0 * (inside / outside.max(1e-30)).log10();
        assert!(db > 30.0, "only {db:.1} dB between the tone in the passband and the one outside it");
    }

    use super::*;

    /// Span rates the chain must work at: the odd one is the real case.
    const RATES: [f64; 3] = [48_077.0, 48_000.0, 24_038.5];

    /// A complex exponential at `freq` Hz (negative = below the carrier).
    fn tone(fs: f64, freq: f64, amp: f32, seconds: f64) -> Vec<Complex32> {
        let n = (seconds * fs) as usize;
        (0..n)
            .map(|i| {
                let ph = 2.0 * PI * freq * i as f64 / fs;
                Complex32::new(amp * ph.cos() as f32, amp * ph.sin() as f32)
            })
            .collect()
    }

    /// Complex Gaussian noise with standard deviation `sigma` per rail.
    fn noise(fs: f64, sigma: f32, seconds: f64, seed: u64) -> Vec<Complex32> {
        let n = (seconds * fs) as usize;
        let mut s = seed | 1;
        let mut uniform = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            ((s >> 11) as f64 + 0.5) / (1u64 << 53) as f64
        };
        (0..n)
            .map(|_| {
                let r = (-2.0 * uniform().ln()).sqrt();
                let a = 2.0 * PI * uniform();
                Complex32::new(sigma * (r * a.cos()) as f32, sigma * (r * a.sin()) as f32)
            })
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| (v * v) as f64).sum::<f64>() / x.len().max(1) as f64).sqrt() as f32
    }

    fn db(x: f32) -> f32 {
        20.0 * (x.max(1e-20)).log10()
    }

    /// Amplitude of the sinusoid at `freq` in `x` (Goertzel).
    fn goertzel_amp(x: &[f32], freq: f64, fs: f64) -> f32 {
        let w = 2.0 * PI * freq / fs;
        let c = 2.0 * w.cos();
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        for &v in x {
            let s0 = v as f64 + c * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let power = s1 * s1 + s2 * s2 - c * s1 * s2;
        (2.0 * power.max(0.0).sqrt() / x.len() as f64) as f32
    }

    /// Frequency from the count of rising zero crossings.
    fn zero_crossing_hz(x: &[f32], fs: f64) -> f64 {
        let rising = x.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        rising as f64 * fs / x.len() as f64
    }

    /// Steady-state tail of a run, skipping the filter and AGC transients.
    fn tail(x: &[f32], fs: f64, seconds: f64) -> &[f32] {
        &x[x.len() - (seconds * fs) as usize..]
    }

    /// Pre-AGC RMS for a tone at `freq` through a fresh demodulator.
    fn raw_rms(fs: f64, sb: Sideband, freq: f64) -> f32 {
        let mut d = SsbDemod::new(fs, sb);
        let mut out = Vec::new();
        d.process_raw(&tone(fs, freq, 0.5, 0.4), &mut out);
        rms(tail(&out, fs, 0.25))
    }

    #[test]
    fn usb_tone_comes_out_at_its_audio_frequency() {
        for fs in RATES {
            let mut d = SsbDemod::new(fs, Sideband::Upper);
            let mut out = Vec::new();
            d.process(&tone(fs, 1000.0, 0.1, 1.0), &mut out);
            let t = tail(&out, fs, 0.5);
            let hz = zero_crossing_hz(t, fs);
            let amp = goertzel_amp(t, 1000.0, fs);
            assert!((hz - 1000.0).abs() < 3.0, "fs {fs}: got {hz:.1} Hz");
            assert!(
                (0.5..0.75).contains(&amp),
                "fs {fs}: 1 kHz tone at {amp:.3}, expected near {TARGET_PEAK}"
            );
            // Nearly all the output power is that one tone.
            assert!(db(amp / 2f32.sqrt()) - db(rms(t)) > -0.1);
        }
    }

    #[test]
    fn lsb_tone_comes_out_upright() {
        for fs in RATES {
            let mut d = SsbDemod::new(fs, Sideband::Lower);
            let mut out = Vec::new();
            d.process(&tone(fs, -1000.0, 0.1, 1.0), &mut out);
            let t = tail(&out, fs, 0.5);
            let hz = zero_crossing_hz(t, fs);
            let amp = goertzel_amp(t, 1000.0, fs);
            // Not 2100 Hz (inverted around the passband center).
            assert!((hz - 1000.0).abs() < 3.0, "fs {fs}: got {hz:.1} Hz");
            assert!((0.5..0.75).contains(&amp), "fs {fs}: level {amp:.3}");
        }
    }

    /// Pre-AGC sweep of both sidebands: passband flatness over 300–2800 Hz,
    /// and the opposite sideband's level relative to it.
    #[test]
    fn opposite_sideband_is_rejected_by_40_db() {
        for fs in RATES {
            for sb in [Sideband::Upper, Sideband::Lower] {
                let sign = if sb == Sideband::Upper { 1.0 } else { -1.0 };
                let freqs: Vec<f64> = (3..=28).map(|k| k as f64 * 100.0).collect();
                let pass: Vec<f32> =
                    freqs.iter().map(|&f| db(raw_rms(fs, sb, sign * f))).collect();
                let max = pass.iter().cloned().fold(f32::MIN, f32::max);
                let min = pass.iter().cloned().fold(f32::MAX, f32::min);
                let ripple = max - min;
                let worst_reject = freqs
                    .iter()
                    .map(|&f| min - db(raw_rms(fs, sb, -sign * f)))
                    .fold(f32::MAX, f32::min);
                eprintln!(
                    "fs {fs} {sb:?}: passband ripple {ripple:.2} dB, \
                     worst opposite-sideband rejection {worst_reject:.1} dB"
                );
                assert!(ripple < 1.0, "fs {fs} {sb:?}: ripple {ripple:.2} dB");
                assert!(
                    worst_reject >= 40.0,
                    "fs {fs} {sb:?}: rejection only {worst_reject:.1} dB"
                );
            }
        }
    }

    #[test]
    fn opposite_1khz_tone_is_rejected_by_40_db() {
        for fs in RATES {
            let usb_wanted = raw_rms(fs, Sideband::Upper, 1000.0);
            let usb_image = raw_rms(fs, Sideband::Upper, -1000.0);
            let lsb_wanted = raw_rms(fs, Sideband::Lower, -1000.0);
            let lsb_image = raw_rms(fs, Sideband::Lower, 1000.0);
            let usb = db(usb_wanted) - db(usb_image);
            let lsb = db(lsb_wanted) - db(lsb_image);
            eprintln!("fs {fs}: 1 kHz image rejection USB {usb:.1} dB, LSB {lsb:.1} dB");
            assert!(usb >= 40.0 && lsb >= 40.0, "fs {fs}: USB {usb:.1}, LSB {lsb:.1}");
            // Pre-AGC level is the input level, within the ripple.
            assert!((db(usb_wanted) - db(0.5 / 2f32.sqrt())).abs() < 1.0);
        }
    }

    #[test]
    fn dc_offset_and_a_zero_hz_carrier_do_not_come_through() {
        for fs in RATES {
            let input = vec![Complex32::new(0.3, -0.2); (2.0 * fs) as usize];
            let mut d = SsbDemod::new(fs, Sideband::Upper);
            let mut raw = Vec::new();
            d.process_raw(&input, &mut raw);
            let mut d = SsbDemod::new(fs, Sideband::Upper);
            let mut out = Vec::new();
            d.process(&input, &mut out);
            let raw_tail = rms(tail(&raw, fs, 1.0));
            let out_tail = rms(tail(&out, fs, 1.0));
            assert!(raw_tail < 1e-5, "fs {fs}: raw residue {raw_tail:e}");
            // Even at the full 60 dB of AGC gain.
            assert!(out_tail < 1e-3, "fs {fs}: audio residue {out_tail:e}");
        }
    }

    #[test]
    fn silence_and_full_scale_noise_stay_finite_and_bounded() {
        let fs = 48_077.0;
        let mut d = SsbDemod::new(fs, Sideband::Upper);
        let mut out = Vec::new();
        d.process(&vec![Complex32::new(0.0, 0.0); fs as usize], &mut out);
        assert!(out.iter().all(|v| *v == 0.0));

        for sb in [Sideband::Upper, Sideband::Lower] {
            let mut d = SsbDemod::new(fs, sb);
            d.process(&noise(fs, 1.0, 3.0, 7), &mut out);
            assert!(out.iter().all(|v| v.is_finite() && v.abs() <= CLAMP));
            let level = rms(tail(&out, fs, 1.0));
            assert!(level < 0.5, "{sb:?}: noise plays at {level:.2} RMS");
        }
    }

    #[test]
    fn agc_brings_0_db_and_minus_40_db_together() {
        let fs = 48_077.0;
        let level = |amp: f32| {
            let mut d = SsbDemod::new(fs, Sideband::Upper);
            let mut out = Vec::new();
            d.process(&tone(fs, 1000.0, amp, 1.0), &mut out);
            goertzel_amp(tail(&out, fs, 0.5), 1000.0, fs)
        };
        let loud = level(1.0);
        let quiet = level(0.01);
        eprintln!("AGC: 0 dB tone → {loud:.3}, −40 dB tone → {quiet:.3}");
        assert!((db(loud) - db(quiet)).abs() < 2.0);

        // And the same demodulator recovering after a loud over: hang, then
        // 15 dB/s, covers 40 dB in ~3.1 s.
        let mut d = SsbDemod::new(fs, Sideband::Upper);
        let mut input = tone(fs, 1000.0, 1.0, 1.0);
        input.extend(tone(fs, 1000.0, 0.01, 4.5));
        let mut out = Vec::new();
        d.process(&input, &mut out);
        let recovered = goertzel_amp(tail(&out, fs, 0.5), 1000.0, fs);
        assert!(
            (db(recovered) - db(loud)).abs() < 3.0,
            "recovered to {recovered:.3} vs {loud:.3}"
        );
    }

    #[test]
    fn agc_holds_through_the_hang_then_decays_slowly() {
        let fs = 48_000.0;
        let mut d = SsbDemod::new(fs, Sideband::Upper);
        let mut input = tone(fs, 1000.0, 1.0, 0.5);
        input.extend(tone(fs, 1000.0, 0.01, 1.0));
        let mut out = Vec::new();
        d.process(&input, &mut out);
        let loud = goertzel_amp(&out[(0.3 * fs) as usize..(0.5 * fs) as usize], 1000.0, fs);
        // 0.1–0.3 s into the quiet part: still inside the hang.
        let held = goertzel_amp(&out[(0.6 * fs) as usize..(0.8 * fs) as usize], 1000.0, fs);
        // 1.4–1.5 s: hang over at 0.9 s, so ~0.55 s × 15 dB/s ≈ 8 dB back.
        let late = goertzel_amp(&out[(1.4 * fs) as usize..(1.5 * fs) as usize], 1000.0, fs);
        let held_db = db(held) - db(loud);
        let late_db = db(late) - db(loud);
        eprintln!("hang: {held_db:.1} dB, after decay {late_db:.1} dB");
        assert!((held_db + 40.0).abs() < 1.5, "held at {held_db:.1} dB");
        assert!((-36.0..-28.0).contains(&late_db), "decayed to {late_db:.1} dB");
    }

    #[test]
    fn a_sudden_over_does_not_clip() {
        let fs = 48_077.0;
        let mut d = SsbDemod::new(fs, Sideband::Upper);
        let mut input = vec![Complex32::new(0.0, 0.0); (0.5 * fs) as usize];
        input.extend(tone(fs, 1000.0, 1.0, 0.3));
        let mut out = Vec::new();
        d.process(&input, &mut out);
        let max = out.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        eprintln!("onset peak {max:.3}");
        assert!(max < 1.0, "onset peaked at {max:.3}");
    }

    #[test]
    fn noise_between_overs_sits_well_below_speech() {
        let fs = 48_077.0;
        let mut d = SsbDemod::new(fs, Sideband::Upper);
        let mut out = Vec::new();
        // Roughly a −80 dBFS passband noise floor.
        d.process(&noise(fs, 1e-4 / 2f32.sqrt() * 4.4, 3.0, 3), &mut out);
        let hiss = rms(tail(&out, fs, 1.0));
        d.reset();
        d.process(&tone(fs, 1000.0, 0.01, 1.0), &mut out);
        let voice = rms(tail(&out, fs, 0.5));
        eprintln!("hiss {:.1} dB under a −40 dBFS tone", db(voice) - db(hiss));
        assert!(db(voice) - db(hiss) > 12.0);
    }

    /// A weak signal on a quiet band: a −90 dBFS tone some 20 dB over the
    /// hiss needs about 86 dB of gain, far past the fixed 60. The floor
    /// tracker sees how quiet the band is and allows it, while the hiss
    /// between overs still plays well under the speech. The tone is keyed
    /// like speech, so the gaps keep the floor honest.
    #[test]
    fn a_weak_signal_on_a_quiet_band_is_lifted_past_60_db() {
        let fs = 48_077.0;
        let sigma = 5e-6;
        let mut d = SsbDemod::new(fs, Sideband::Upper);
        let mut out = Vec::new();
        d.process(&noise(fs, sigma, 3.0, 11), &mut out);
        let hiss = rms(tail(&out, fs, 1.0));
        let gain_db = d.max_gain_db();
        let hush = noise(fs, sigma, 2.5, 12);
        let key = (0.5 * fs) as usize;
        let input: Vec<Complex32> = tone(fs, 1000.0, 3e-5, 2.5)
            .iter()
            .zip(&hush)
            .enumerate()
            .map(|(i, (t, n))| if i % key < key * 3 / 5 { t + n } else { *n })
            .collect();
        d.process(&input, &mut out);
        // The last keyed-on stretch, past its onset.
        let on = &out[4 * key + key / 5..4 * key + key * 3 / 5];
        let amp = goertzel_amp(on, 1000.0, fs);
        let voice = rms(on);
        eprintln!(
            "quiet band: ceiling {gain_db:.1} dB, tone at {amp:.3}, hiss {:.1} dB under it",
            db(voice) - db(hiss)
        );
        assert!(gain_db > 80.0, "ceiling only {gain_db:.1} dB on a quiet band");
        assert!((0.45..0.75).contains(&amp), "weak tone played at {amp:.3}");
        assert!(db(voice) - db(hiss) > 12.0);
    }

    /// A noisy band keeps the fixed ceiling: its hiss is above −64 dBFS, so
    /// the floor tracker never lifts the gain past 60 dB.
    #[test]
    fn a_noisy_band_keeps_the_fixed_ceiling() {
        let fs = 48_077.0;
        let mut d = SsbDemod::new(fs, Sideband::Upper);
        let mut out = Vec::new();
        d.process(&noise(fs, 1e-3, 3.0, 13), &mut out);
        assert!((d.max_gain_db() - SSB_MAX_GAIN_DB).abs() < 0.01, "ceiling {:.1} dB", d.max_gain_db());
    }

    #[test]
    fn the_oscillator_does_not_drift_over_a_long_run() {
        let fs = 48_077.0;
        let mut d = SsbDemod::new(fs, Sideband::Upper);
        let mut out = Vec::new();
        // ~90 s of tone, in blocks as the server would deliver it.
        let block = tone(fs, 1000.0, 0.1, 1.0);
        let mut phase_input = Vec::with_capacity(block.len());
        for second in 0..90 {
            // Continue the tone's phase across blocks.
            let off = second as f64 * block.len() as f64;
            phase_input.clear();
            phase_input.extend((0..block.len()).map(|i| {
                let ph = 2.0 * PI * 1000.0 * (off + i as f64) / fs;
                Complex32::new(0.1 * ph.cos() as f32, 0.1 * ph.sin() as f32)
            }));
            d.process(&phase_input, &mut out);
        }
        assert!((d.phasor.norm() - 1.0).abs() < 1e-4);
        let hz = zero_crossing_hz(&out, fs);
        let amp = goertzel_amp(&out, 1000.0, fs);
        assert!((hz - 1000.0).abs() < 3.0, "after 90 s: {hz:.1} Hz");
        assert!((0.5..0.75).contains(&amp), "after 90 s: level {amp:.3}");
    }

    #[test]
    fn switching_sideband_moves_the_passband() {
        let fs = 48_077.0;
        let mut d = SsbDemod::new(fs, Sideband::Upper);
        d.set_sideband(Sideband::Lower);
        assert_eq!(d.sideband(), Sideband::Lower);
        let mut out = Vec::new();
        d.process_raw(&tone(fs, -1000.0, 0.5, 0.4), &mut out);
        let lsb = rms(tail(&out, fs, 0.25));
        d.reset();
        d.process_raw(&tone(fs, 1000.0, 0.5, 0.4), &mut out);
        let usb = rms(tail(&out, fs, 0.25));
        assert!(db(lsb) - db(usb) > 40.0);
    }
}
