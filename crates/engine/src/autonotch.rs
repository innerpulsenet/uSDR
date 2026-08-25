//! Adaptive audio auto-notch for the monitor path.
//!
//! A stable interfering tone sitting on top of a weak signal is the classic
//! reason a marginal carrier becomes unreadable. This tracks the strongest
//! persistent narrowband components in 400–2800 Hz and nulls each with a
//! biquad whose depth follows how consistently the tone has been present.
//! "Persistent" is the operative filter: speech energy moves constantly and
//! never earns a notch, while a heterodyne sits in one bin for seconds.

use std::f32::consts::TAU;

/// One tracked candidate tone.
struct Candidate {
    freq_hz: f32,
    /// Smoothed dominance score; the notch depth follows it directly.
    score: f32,
}

/// A placed biquad notch with its own state.
struct Notch {
    w0: f32,
    r: f32,
    depth: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Notch {
    fn new(freq_hz: f32, fs: f32, depth: f32) -> Self {
        Self {
            w0: TAU * freq_hz / fs,
            r: 0.97,
            depth,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        // Zeroes at exp(±jw0): 1 - 2cos(w0) z^-1 + z^-2
        // Poles at r·exp(±jw0): denominator (1 - 2r·cos(w0) z^-1 + r² z^-2)
        let ff = x - 2.0 * self.w0.cos() * self.x1 + self.x2;
        let y = ff + 2.0 * self.r * self.w0.cos() * self.y1 - self.r * self.r * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        // Blend by depth: depth 1 = fully nulled output.
        x * (1.0 - self.depth) + y * self.depth
    }
}

pub struct AutoNotch {
    fs: f32,
    candidates: Vec<Candidate>,
    notches: Vec<Notch>,
    sample_counter: usize,
    /// Rolling window for the periodic spectral scan.
    window: Vec<f32>,
}

const SCAN_INTERVAL: usize = 2048;
const WINDOW_LEN: usize = 1024;
const MIN_HZ: f32 = 400.0;
const MAX_HZ: f32 = 2_800.0;
const BINS: usize = 128;
/// Share of in-band energy a bin must hold to count as a candidate peak.
const PEAK_SHARE: f32 = 0.12;
/// Score above which a candidate earns a full notch.
const OPEN_SCORE: f32 = 0.6;
/// Below this the notch releases.
const CLOSE_SCORE: f32 = 0.25;
const MAX_NOTCHES: usize = 3;

impl AutoNotch {
    pub fn new(fs_hz: f64) -> Self {
        Self {
            fs: fs_hz as f32,
            candidates: Vec::new(),
            notches: Vec::new(),
            sample_counter: 0,
            window: Vec::with_capacity(WINDOW_LEN),
        }
    }

    pub fn active(&self) -> bool {
        !self.notches.is_empty()
    }

    pub fn process(&mut self, x: &mut [f32]) {
        for &s in x.iter() {
            if self.window.len() == WINDOW_LEN {
                self.window.remove(0);
            }
            self.window.push(s);
            self.sample_counter += 1;
            if self.sample_counter % SCAN_INTERVAL == 0 && self.window.len() >= WINDOW_LEN / 2 {
                self.rescan();
                break;
            }
        }
        for s in x.iter_mut() {
            let mut v = *s;
            for n in &mut self.notches {
                v = n.process(v);
            }
            *s = v;
        }
    }

    /// Coarse Goertzel scan of the analysis window; update candidate scores
    /// and re-place the notches.
    fn rescan(&mut self) {
        let nyq = self.fs / 2.0;
        let lo_bin = ((MIN_HZ / nyq) * BINS as f32).ceil() as usize;
        let hi_bin = (((MAX_HZ / nyq) * BINS as f32) as usize).min(BINS - 2);
        let mut power = [0.0f32; BINS];
        for b in lo_bin..=hi_bin {
            // Exact-frequency Goertzel: the coefficient is 2cos(2πf/fs),
            // no integer-bin rounding needed. Rounding to DFT bins made the
            // detector miss tones sitting between bins.
            let hz = b as f32 / BINS as f32 * nyq;
            let coeff = 2.0 * (TAU * hz / self.fs).cos();
            let (mut q1, mut q2) = (0.0f32, 0.0f32);
            for &s in &self.window {
                let q0 = s + coeff * q1 - q2;
                q2 = q1;
                q1 = q0;
            }
            power[b] = q1 * q1 + q2 * q2 - coeff * q1 * q2;
        }
        let total: f32 = power[lo_bin..=hi_bin].iter().sum::<f32>().max(1e-12);

        let mut found: Vec<(f32, f32)> = Vec::new();
        for b in lo_bin..hi_bin {
            if power[b] > power[b - 1]
                && power[b] >= power[b + 1]
                && power[b] > total * PEAK_SHARE
            {
                let hz = b as f32 / BINS as f32 * nyq;
                found.push((hz, power[b] / total));
            }
        }
        found.sort_by(|a, b| b.1.total_cmp(&a.1));

        // Decay every candidate; reinforce those re-found this scan.
        for c in &mut self.candidates {
            c.score *= 0.6;
        }
        for &(hz, share) in found.iter().take(MAX_NOTCHES) {
            match self
                .candidates
                .iter_mut()
                .find(|c| (c.freq_hz - hz).abs() < 80.0)
            {
                Some(c) => {
                    c.freq_hz = c.freq_hz * 0.5 + hz * 0.5;
                    c.score = (c.score + share * 0.8).min(1.5);
                }
                None => self.candidates.push(Candidate { freq_hz: hz, score: share }),
            }
        }
        self.candidates.retain(|c| c.score > 0.05);

        // Re-place notches from persistent candidates.
        self.notches.clear();
        for c in self
            .candidates
            .iter()
            .filter(|c| c.score > CLOSE_SCORE)
            .take(MAX_NOTCHES)
        {
            let depth = ((c.score - CLOSE_SCORE) / (OPEN_SCORE - CLOSE_SCORE)).clamp(0.0, 1.0);
            self.notches.push(Notch::new(c.freq_hz, self.fs, depth));
        }
    }
}
