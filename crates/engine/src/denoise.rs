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
//! Latency is one frame plus one hop, about 16 ms at 48 kHz. `Off` passes the
//! audio straight through with no latency at all.

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
}

impl Denoise {
    pub fn new(fs: f64) -> Self {
        // About 10 ms: long enough to resolve voice harmonics (~94 Hz bins at
        // 48 kHz), short enough that a syllable's onset is not smeared.
        let n = ((fs * 0.0107).round() as usize).next_power_of_two().clamp(64, 2048);
        let hop = n / 2;
        let bins = n / 2 + 1;
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
        }
    }

    pub fn level(&self) -> NrLevel {
        self.level
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
            *x = self.out.pop_front().unwrap_or(0.0);
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
        for k in 0..bins {
            let g = self.gain[k];
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
}
