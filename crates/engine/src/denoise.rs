//! Spectral noise reduction for monitored voice.
//!
//! A short-time Wiener filter: the audio is cut into 50%-overlapped frames of
//! about 10 ms, each bin's noise power is estimated, and each bin is scaled by
//! how much of it is signal. Three choices keep it from sounding like a
//! filter:
//!
//! - **Noise from minima, not from silence.** A bin's noise floor is the
//!   lowest its smoothed power has been over the last ~1.5 s (minimum
//!   statistics). Speech never fills a bin for that long, so the estimate
//!   tracks a changing band without needing a pause to learn it, and a
//!   steady carrier is not mistaken for noise until it has sat for seconds.
//! - **Decision-directed gain.** The a-priori SNR leans on the previous
//!   frame's cleaned output, so the gain moves smoothly rather than flickering
//!   bin by bin — the flicker is what makes crude spectral subtraction warble
//!   ("musical noise").
//! - **A floor.** No bin is cut below the level's floor, so the residual hiss
//!   is quieter but still natural, and weak speech is never gated away.
//!
//! - **Makeup gain.** The demodulator's AGC set the level from speech *and*
//!   noise; take the noise away and the voice is left quieter than it
//!   played before. While speech is present, the frame's power before and
//!   after cleaning is averaged, and the difference is given back (up to
//!   18 dB), so turning noise reduction up never turns the voice down.
//!   Between overs the makeup holds for a moment, then eases back to unity
//!   so the residual hiss stays where the floor put it. A soft limiter keeps
//!   the restored peaks under full scale.
//!
//! Latency is exactly one frame, 512 samples (10.7 ms) at 48 kHz; a signal
//! that must stay in step with the audio (the squelch's noise band) goes
//! through [`Denoise::delay_side`]. `Off` passes the audio straight through
//! with no latency at all.

use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NrLevel {
    #[default]
    Off,
    Low,
    Medium,
    High,
}

impl NrLevel {
    /// (noise over-estimate, gain floor in dB)
    fn params(self) -> (f32, f32) {
        match self {
            NrLevel::Off => (1.0, 0.0),
            NrLevel::Low => (1.0, -8.0),
            NrLevel::Medium => (1.5, -14.0),
            NrLevel::High => (2.5, -20.0),
        }
    }
}

/// Frames of smoothed power per minimum sub-window, and how many sub-windows
/// the minimum spans: 8 × 32 hops of 5.3 ms is about 1.4 s.
const SUB_FRAMES: u32 = 32;
const SUBWINDOWS: usize = 8;
/// The minimum of a smoothed noise power sits below its mean; this restores it.
const MIN_BIAS: f32 = 1.6;
/// Decision-directed weight on the previous frame.
const DD_ALPHA: f32 = 0.97;
/// Power smoothing before the minimum is taken.
const SMOOTH: f32 = 0.85;

/// The voice band that speech presence and makeup are measured over, Hz.
const VOICE_LO_HZ: f32 = 250.0;
const VOICE_HI_HZ: f32 = 3_000.0;
/// A frame holds speech when its power in the voice band stands this far
/// over the noise estimate there (3.4 dB). The estimate sits a little under
/// the true mean, so hiss alone reads about 1.5; voice 0 dB over the hiss
/// reads near 3.
const SPEECH_OVER: f32 = 2.2;
/// Consecutive frames over the line before they count as speech (16 ms).
const SPEECH_RUN: u32 = 3;
/// Most makeup ever given back, dB.
const MAKEUP_MAX_DB: f32 = 18.0;
/// Speech frames' power before and after cleaning is averaged over about
/// this long, so the makeup follows the talker, not the syllable.
const MAKEUP_AVG_S: f32 = 0.25;
/// Makeup climbs toward its target at this rate and falls at the second;
/// after `MAKEUP_HANG_S` without speech it eases back to unity at the third.
const MAKEUP_RISE_DB_PER_S: f32 = 30.0;
const MAKEUP_FALL_DB_PER_S: f32 = 120.0;
const MAKEUP_HANG_S: f32 = 0.8;
const MAKEUP_RELEASE_DB_PER_S: f32 = 4.0;
/// The soft limiter is linear to here and bends smoothly toward
/// `LIMIT_CEILING` above it.
const LIMIT_KNEE: f32 = 0.8;
const LIMIT_CEILING: f32 = 0.98;

pub struct Denoise {
    level: NrLevel,
    n: usize,
    hop: usize,
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    win: Vec<f32>,
    /// The frame being filled: the previous hop, then the new one.
    input: Vec<f32>,
    since_frame: usize,
    /// Overlap-add accumulator, `n` long; the first `hop` are complete after
    /// each frame.
    acc: Vec<f32>,
    out: VecDeque<f32>,
    /// The side signal held back by the frame's latency; empty until the
    /// first side sample arrives, see `delay_side`.
    side: VecDeque<f32>,
    spec: Vec<Complex32>,
    /// Per bin, 0..=n/2.
    smooth: Vec<f32>,
    sub_min: Vec<f32>,
    hist: Vec<Vec<f32>>,
    hist_at: usize,
    sub_count: u32,
    frames: u32,
    noise: Vec<f32>,
    prev_clean: Vec<f32>,
    gain: Vec<f32>,
    /// Bins covering `VOICE_LO_HZ..VOICE_HI_HZ`.
    voice: std::ops::Range<usize>,
    /// Speech frames' averaged power in and out, the makeup applied (dB),
    /// and the frames left before it starts easing back.
    speech_in: f32,
    speech_out: f32,
    makeup_db: f32,
    hang: u32,
    /// Consecutive frames over the speech line.
    run: u32,
    /// Per-frame steps: the averaging weight, rise, fall and release (dB),
    /// and the hang length in frames.
    avg_k: f32,
    rise_db: f32,
    fall_db: f32,
    release_db: f32,
    hang_frames: u32,
}

impl Denoise {
    pub fn new(fs: f64) -> Self {
        // About 10 ms: long enough to resolve voice harmonics (~94 Hz bins at
        // 48 kHz), short enough that a syllable's onset is not smeared. The
        // nearest power of two, not the next one up: 10.7 ms at 48 kHz is
        // 513.6 samples, which `next_power_of_two` turned into 1024, a 21 ms
        // frame that doubled the latency and the noise window.
        let n = (1usize << (fs * 0.0107).max(1.0).log2().round() as u32).clamp(64, 2048);
        let hop = n / 2;
        let bins = n / 2 + 1;
        let hop_s = hop as f32 / fs as f32;
        let bin_hz = fs as f32 / n as f32;
        let voice = ((VOICE_LO_HZ / bin_hz).round() as usize).max(1)..((VOICE_HI_HZ / bin_hz).round() as usize + 1).min(bins);
        let mut planner = FftPlanner::new();
        // Periodic sqrt-Hann on analysis and synthesis: the product is a Hann,
        // and Hann windows at 50% overlap sum to exactly one.
        let win = (0..n)
            .map(|i| (0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / n as f32).cos()).sqrt())
            .collect();
        Self {
            level: NrLevel::Off,
            n,
            hop,
            fft: planner.plan_fft_forward(n),
            ifft: planner.plan_fft_inverse(n),
            win,
            input: vec![0.0; n],
            since_frame: 0,
            acc: vec![0.0; n],
            out: VecDeque::from(vec![0.0; hop]),
            side: VecDeque::with_capacity(n + 1),
            spec: vec![Complex32::new(0.0, 0.0); n],
            smooth: vec![0.0; bins],
            sub_min: vec![f32::MAX; bins],
            hist: vec![vec![f32::MAX; bins]; SUBWINDOWS],
            hist_at: 0,
            sub_count: 0,
            frames: 0,
            noise: vec![0.0; bins],
            prev_clean: vec![0.0; bins],
            gain: vec![1.0; bins],
            voice,
            speech_in: 0.0,
            speech_out: 0.0,
            makeup_db: 0.0,
            hang: 0,
            run: 0,
            avg_k: (hop_s / MAKEUP_AVG_S).min(1.0),
            rise_db: MAKEUP_RISE_DB_PER_S * hop_s,
            fall_db: MAKEUP_FALL_DB_PER_S * hop_s,
            release_db: MAKEUP_RELEASE_DB_PER_S * hop_s,
            hang_frames: (MAKEUP_HANG_S / hop_s).round() as u32,
        }
    }

    /// The makeup gain being given back, dB.
    pub fn makeup_db(&self) -> f32 {
        self.makeup_db
    }

    pub fn level(&self) -> NrLevel {
        self.level
    }

    /// Samples the cleaned audio trails its input by: one frame, or none
    /// when off.
    pub fn latency(&self) -> usize {
        if self.level == NrLevel::Off { 0 } else { self.n }
    }

    /// Delays `side` by [`Self::latency`] into `out` (cleared first), so a
    /// signal measured on the undelayed audio lines up with what `process`
    /// returns. The NFM gate needs it: keyed on the discriminator's noise
    /// band while the voice ran a frame late, it opened a frame before the
    /// voice arrived (letting the pre-key hiss through) and closed a frame
    /// before the last of it left. The line starts filled with the first
    /// side sample rather than zeros, because a zero noise band reads as a
    /// perfectly quieted carrier.
    pub fn delay_side(&mut self, side: &[f32], out: &mut Vec<f32>) {
        out.clear();
        if self.level == NrLevel::Off {
            out.extend_from_slice(side);
            return;
        }
        if self.side.is_empty()
            && let Some(&first) = side.first()
        {
            self.side.extend(std::iter::repeat_n(first, self.n));
        }
        for &v in side {
            self.side.push_back(v);
            out.push(self.side.pop_front().unwrap_or(v));
        }
    }

    /// Change the strength. Turning it on starts from a clean slate, so the
    /// first second learns the band rather than applying a stale estimate.
    pub fn set_level(&mut self, level: NrLevel) {
        if level != self.level && self.level == NrLevel::Off {
            self.reset();
        }
        self.level = level;
    }

    pub fn reset(&mut self) {
        self.input.fill(0.0);
        self.since_frame = 0;
        self.acc.fill(0.0);
        self.out.clear();
        self.out.extend(std::iter::repeat_n(0.0, self.hop));
        self.side.clear();
        self.smooth.fill(0.0);
        self.sub_min.fill(f32::MAX);
        for h in &mut self.hist {
            h.fill(f32::MAX);
        }
        self.sub_count = 0;
        self.frames = 0;
        self.noise.fill(0.0);
        self.prev_clean.fill(0.0);
        self.gain.fill(1.0);
        self.speech_in = 0.0;
        self.speech_out = 0.0;
        self.makeup_db = 0.0;
        self.hang = 0;
        self.run = 0;
    }

    /// Clean `audio` in place.
    pub fn process(&mut self, audio: &mut [f32]) {
        if self.level == NrLevel::Off {
            return;
        }
        for x in audio.iter_mut() {
            // New samples fill the frame's back half; once it is full the
            // frame runs and the half slides forward.
            self.input[self.n - self.hop + self.since_frame] = *x;
            self.since_frame += 1;
            if self.since_frame == self.hop {
                self.since_frame = 0;
                self.frame();
                self.input.copy_within(self.hop.., 0);
            }
            *x = soft_limit(self.out.pop_front().unwrap_or(0.0));
        }
    }

    fn frame(&mut self) {
        let n = self.n;
        let bins = n / 2 + 1;
        for i in 0..n {
            self.spec[i] = Complex32::new(self.input[i] * self.win[i], 0.0);
        }
        self.fft.process(&mut self.spec);
        let (over, floor_db) = self.level.params();
        let floor = 10f32.powf(floor_db / 20.0);
        let first = self.frames == 0;
        for k in 0..bins {
            let p = self.spec[k].norm_sqr();
            self.smooth[k] = if first { p } else { SMOOTH * self.smooth[k] + (1.0 - SMOOTH) * p };
            self.sub_min[k] = self.sub_min[k].min(self.smooth[k]);
        }
        self.sub_count += 1;
        if self.sub_count >= SUB_FRAMES {
            self.sub_count = 0;
            std::mem::swap(&mut self.hist[self.hist_at], &mut self.sub_min);
            self.sub_min.fill(f32::MAX);
            self.hist_at = (self.hist_at + 1) % SUBWINDOWS;
        }
        for k in 0..bins {
            let m = self
                .hist
                .iter()
                .map(|h| h[k])
                .fold(self.sub_min[k], f32::min);
            // Until a full sub-window has passed, the running minimum is all
            // there is; it settles from above as quiet frames arrive.
            self.noise[k] = (m * MIN_BIAS).max(1e-12);
        }
        for k in 0..bins {
            let p = self.spec[k].norm_sqr();
            let noise = self.noise[k] * over;
            let post = p / noise;
            let prior = if first {
                (post - 1.0).max(0.0)
            } else {
                DD_ALPHA * self.prev_clean[k] / noise + (1.0 - DD_ALPHA) * (post - 1.0).max(0.0)
            };
            let g = (prior / (1.0 + prior)).max(floor);
            self.gain[k] = g;
            self.prev_clean[k] = g * g * p;
        }
        // A light three-bin smoothing of the gain across frequency takes the
        // edge off isolated bins that would otherwise twinkle.
        let mut last = self.gain[0];
        for k in 1..bins - 1 {
            let cur = self.gain[k];
            self.gain[k] = 0.25 * last + 0.5 * cur + 0.25 * self.gain[k + 1];
            last = cur;
        }
        self.track_makeup();
        let makeup = 10f32.powf(self.makeup_db / 20.0);
        for k in 0..bins {
            let g = self.gain[k] * makeup;
            self.spec[k] *= g;
            if k > 0 && k < n / 2 {
                self.spec[n - k] *= g;
            }
        }
        self.ifft.process(&mut self.spec);
        let scale = 1.0 / n as f32;
        for i in 0..n {
            self.acc[i] += self.spec[i].re * scale * self.win[i];
        }
        self.out.extend(self.acc[..self.hop].iter().copied());
        self.acc.copy_within(self.hop.., 0);
        let tail = n - self.hop;
        self.acc[tail..].fill(0.0);
        self.frames = self.frames.saturating_add(1);
    }

    /// Decide whether this frame holds speech, and move the makeup gain:
    /// toward what restores the speech frames' power while there is speech,
    /// held for the hang after it, then easing back to unity.
    fn track_makeup(&mut self) {
        let (mut p_in, mut p_out, mut noise) = (0.0f32, 0.0f32, 0.0f32);
        for k in self.voice.clone() {
            let p = self.spec[k].norm_sqr();
            p_in += p;
            p_out += self.gain[k] * self.gain[k] * p;
            noise += self.noise[k];
        }
        // Until the minimum spans its full window the noise estimate runs
        // low; nothing counts as speech before then.
        let settled = self.frames >= SUB_FRAMES * SUBWINDOWS as u32;
        // Hiss alone crosses the line on a frame in twenty or so; speech
        // stays over it for syllables. Only a run of frames counts.
        if settled && p_in > SPEECH_OVER * noise && p_out > 0.0 {
            self.run = self.run.saturating_add(1);
        } else {
            self.run = 0;
        }
        let speech = self.run >= SPEECH_RUN;
        if speech {
            self.speech_in += (p_in - self.speech_in) * self.avg_k;
            self.speech_out += (p_out - self.speech_out) * self.avg_k;
            self.hang = self.hang_frames;
            let want = if self.speech_out > 0.0 {
                (10.0 * (self.speech_in / self.speech_out).log10()).clamp(0.0, MAKEUP_MAX_DB)
            } else {
                0.0
            };
            self.makeup_db += (want - self.makeup_db).clamp(-self.fall_db, self.rise_db);
        } else if self.hang > 0 {
            self.hang -= 1;
        } else {
            self.makeup_db = (self.makeup_db - self.release_db).max(0.0);
        }
    }
}

/// Linear below `LIMIT_KNEE`, bending smoothly toward `LIMIT_CEILING` above
/// it: restored peaks are rounded off instead of clipped.
#[inline]
fn soft_limit(x: f32) -> f32 {
    let a = x.abs();
    if a <= LIMIT_KNEE {
        return x;
    }
    let room = LIMIT_CEILING - LIMIT_KNEE;
    (LIMIT_KNEE + room * ((a - LIMIT_KNEE) / room).tanh()).copysign(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(n: usize, amp: f32) -> Vec<f32> {
        let mut s = 0x9e37_79b9u32;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s as f32 / u32::MAX as f32 - 0.5) * 2.0 * amp
            })
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    /// The frame at 48 kHz is the 512 samples the design calls for, and the
    /// audio and a side signal leave it exactly that late.
    #[test]
    fn latency_is_one_frame_and_the_side_signal_matches_it() {
        let mut nr = Denoise::new(48_000.0);
        nr.set_level(NrLevel::Low);
        assert_eq!(nr.latency(), 512);
        // A loud click after a second of hiss: well over the noise, so it
        // passes near unity and its position is easy to find.
        let mut x = noise(48_000 * 2, 0.01);
        let at = 48_000 + 123;
        x[at] = 0.7;
        let side: Vec<f32> = (0..x.len()).map(|i| if i == at { 1.0 } else { 0.0 }).collect();
        let mut delayed = Vec::new();
        let mut side_out = Vec::new();
        for (a, b) in x.chunks_mut(480).zip(side.chunks(480)) {
            nr.process(a);
            nr.delay_side(b, &mut delayed);
            side_out.extend_from_slice(&delayed);
        }
        let peak = (0..x.len()).max_by(|&i, &j| x[i].abs().total_cmp(&x[j].abs())).unwrap();
        let side_peak = side_out.iter().position(|&v| v == 1.0).unwrap();
        assert_eq!(peak, at + 512, "audio delayed {} samples", peak as i64 - at as i64);
        assert_eq!(side_peak, at + 512);
    }

    #[test]
    fn off_is_a_straight_wire() {
        let mut nr = Denoise::new(48_000.0);
        let mut x = noise(4_800, 0.3);
        let want = x.clone();
        nr.process(&mut x);
        assert_eq!(x, want);
    }

    #[test]
    fn hiss_comes_down_by_the_floor() {
        let fs = 48_000.0;
        let mut nr = Denoise::new(fs);
        nr.set_level(NrLevel::Medium);
        let mut x = noise(48_000 * 4, 0.2);
        let before = rms(&x[48_000 * 3..]);
        for chunk in x.chunks_mut(960) {
            nr.process(chunk);
        }
        let after = rms(&x[48_000 * 3..]);
        let db = 20.0 * (after / before).log10();
        assert!(db < -10.0, "hiss only {db:.1} dB down");
    }

    #[test]
    fn a_tone_in_hiss_keeps_its_level() {
        // A steady tone stands for the voiced part of speech: well above the
        // noise in its own bin, so its gain stays near unity.
        let fs = 48_000.0f32;
        let mut nr = Denoise::new(fs as f64);
        nr.set_level(NrLevel::High);
        let hiss = noise(48_000 * 3, 0.05);
        let mut x: Vec<f32> = hiss
            .iter()
            .enumerate()
            .map(|(i, h)| {
                // keyed on and off every 300 ms, as speech is
                let on = (i / 14_400) % 2 == 0;
                h + if on { 0.4 * (std::f32::consts::TAU * 700.0 * i as f32 / fs).sin() } else { 0.0 }
            })
            .collect();
        for chunk in x.chunks_mut(512) {
            nr.process(chunk);
        }
        // the last "on" stretch, past the latency and its edges
        let seg = &x[14_400 * 8 + 2_000..14_400 * 9 - 2_000];
        let level = rms(seg) / (0.4 / 2f32.sqrt());
        let db = 20.0 * level.log10();
        assert!(db.abs() < 1.5, "tone moved {db:.1} dB");
    }

    /// Power of the sinusoid at `f` in `x` (Goertzel), as amplitude².
    fn tone_power(x: &[f32], f: f32, fs: f32) -> f32 {
        let c = 2.0 * (std::f32::consts::TAU * f / fs).cos();
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        for &v in x {
            let s0 = v as f64 + c as f64 * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let p = s1 * s1 + s2 * s2 - c as f64 * s1 * s2;
        (4.0 * p.max(0.0) / (x.len() as f64 * x.len() as f64)) as f32
    }

    /// Pure hiss is never mistaken for speech, at any level: no makeup, so
    /// noise reduction still takes the hiss down by its floor.
    #[test]
    fn hiss_alone_gets_no_makeup() {
        for level in [NrLevel::Low, NrLevel::Medium, NrLevel::High] {
            let mut nr = Denoise::new(48_000.0);
            nr.set_level(level);
            let mut x = noise(48_000 * 6, 0.2);
            let mut most = 0.0f32;
            for chunk in x.chunks_mut(960) {
                nr.process(chunk);
                most = most.max(nr.makeup_db());
            }
            eprintln!("{level:?}: most makeup on hiss {most:.1} dB");
            assert!(most < 2.0, "{level:?}: hiss earned {most:.1} dB of makeup");
        }
    }

    /// Weak voice in heavy hiss: a 150 Hz buzz with harmonics across the
    /// voice band, keyed like speech, 2 dB over the hiss in that band. The
    /// Wiener gain alone leaves it some 10 dB quieter than it went in; the
    /// makeup gives that back, so it plays near the level the AGC set.
    #[test]
    fn weak_voice_is_given_its_level_back() {
        let fs = 48_000.0f32;
        let harmonics: Vec<f32> = (2..=18).map(|k| 150.0 * k as f32).collect();
        let a = 0.028f32;
        let hiss = noise(48_000 * 6, 0.3);
        let key = 24_000;
        let voice: Vec<f32> = (0..hiss.len())
            .map(|i| {
                if i % key < key * 3 / 5 {
                    harmonics.iter().map(|f| a * (std::f32::consts::TAU * f * i as f32 / fs).sin()).sum()
                } else {
                    0.0
                }
            })
            .collect();
        for level in [NrLevel::Low, NrLevel::Medium, NrLevel::High] {
            let mut nr = Denoise::new(fs as f64);
            nr.set_level(level);
            let mut x: Vec<f32> = voice.iter().zip(&hiss).map(|(v, h)| v + h).collect();
            let mut makeup = 0.0;
            let start = 11 * key + 2_000;
            for (i, chunk) in x.chunks_mut(960).enumerate() {
                nr.process(chunk);
                if i * 960 == start / 960 * 960 {
                    makeup = nr.makeup_db();
                }
            }
            // The last keyed-on stretch, clear of its edges and the latency.
            let seg = &x[start..start + key * 3 / 5 - 4_000];
            let clean = &voice[start..start + key * 3 / 5 - 4_000];
            let gain_db = 10.0
                * (harmonics.iter().map(|&f| tone_power(seg, f, fs)).sum::<f32>()
                    / harmonics.iter().map(|&f| tone_power(clean, f, fs)).sum::<f32>())
                .log10();
            let peak = x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            eprintln!(
                "{level:?}: voice {gain_db:+.1} dB with {makeup:.1} dB of makeup ({:+.1} dB without), peak {peak:.2}",
                gain_db - makeup
            );
            assert!(makeup > 3.0, "{level:?}: only {makeup:.1} dB of makeup");
            assert!(gain_db > -3.0, "{level:?}: voice came out {gain_db:.1} dB down");
            assert!(gain_db < 3.0, "{level:?}: voice lifted {gain_db:.1} dB");
            assert!(peak <= LIMIT_CEILING, "{level:?}: peaked at {peak:.3}");
        }
    }

    #[test]
    fn the_limiter_is_linear_below_its_knee_and_bounded_above() {
        assert_eq!(soft_limit(0.5), 0.5);
        assert_eq!(soft_limit(-0.8), -0.8);
        assert!(soft_limit(0.9) < 0.9 && soft_limit(0.9) > 0.85);
        assert!(soft_limit(10.0) <= LIMIT_CEILING && soft_limit(-10.0) >= -LIMIT_CEILING);
    }
}
