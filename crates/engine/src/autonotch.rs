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
    /// `2·cos(w0)` — the feedforward coefficient, cached so the per-sample
    /// work is multiplies only.
    c2: f32,
    /// `2·r·cos(w0)` — the feedback coefficient.
    pc2: f32,
    /// `r²`.
    r2: f32,
    depth: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Notch {
    fn new(freq_hz: f32, fs: f32, depth: f32) -> Self {
        let c = (TAU * freq_hz / fs).cos();
        Self {
            c2: 2.0 * c,
            pc2: 2.0 * 0.97 * c,
            r2: 0.97 * 0.97,
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
        let ff = x - self.c2 * self.x1 + self.x2;
        let y = ff + self.pc2 * self.y1 - self.r2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        // Blend by depth: depth 1 = fully nulled output. The pole radius is
        // 0.97, so the resonator's peak gain at w0 is ~1/(1-r) — deep in
        // saturation territory for a full-scale tone. Hard-cap the mix at
        // 1.0: the blend is linear in depth, so a depth driven past it (the
        // score allowed 1.5) does not null "harder", it pushes the blend
        // weight past unity and INVERTS the phase of the tone at the output,
        // which re-injects it as a louder artifact. The score is clamped at
        // its source in `rescan` for the same reason.
        let depth = self.depth.min(1.0);
        x * (1.0 - depth) + y * depth
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
        // Batch the analysis window: extend once and trim once, instead of a
        // per-sample `remove(0)` — an O(n) memmove on the always-on monitor
        // path — and its per-sample modulo.
        if !x.is_empty() {
            let scans_before = self.sample_counter / SCAN_INTERVAL;
            self.sample_counter += x.len();
            self.window.extend_from_slice(x);
            let excess = self.window.len().saturating_sub(WINDOW_LEN);
            if excess > 0 {
                self.window.drain(..excess);
            }
            // Rescan after the whole block has landed in the window. The old
            // in-loop rescan broke out mid-block, so the samples after the
            // scan boundary never reached the analysis window.
            let boundary_crossed = self.sample_counter / SCAN_INTERVAL > scans_before;
            if boundary_crossed && self.window.len() >= WINDOW_LEN / 2 {
                self.rescan();
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
                    // Clamp the reinforcement at 1.0. The score feeds the
                    // notch depth directly (via `(score - CLOSE_SCORE) /
                    // (OPEN_SCORE - CLOSE_SCORE)`), and a depth past 1.0
                    // inverts the notch's blend — see `Notch::process`.
                    c.score = (c.score + share * 0.8).min(1.0);
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
