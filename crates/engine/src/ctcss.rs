//! CTCSS (sub-audible tone) detection by Goertzel bank.
//!
//! A scanner needs this for more than curiosity: several agencies share a
//! dispatch frequency and are separated only by their tone, so the tone is part
//! of a call's identity, not decoration. Litchfield County's NW EMS region uses
//! 192.8 Hz.
//!
//! Goertzel rather than an FFT because only 38 known frequencies matter, and
//! they are not on any convenient bin grid. Evaluating 38 arbitrary points costs
//! far less than an FFT fine enough to separate 71.9 Hz from 74.4 Hz.

/// The 38 standard EIA tones, in Hz.
pub const TONES: [f32; 38] = [
    67.0, 71.9, 74.4, 77.0, 79.7, 82.5, 85.4, 88.5, 91.5, 94.8, 97.4, 100.0, 103.5, 107.2, 110.9,
    114.8, 118.8, 123.0, 127.3, 131.8, 136.5, 141.3, 146.2, 151.4, 156.7, 162.2, 167.9, 173.8,
    179.9, 186.2, 192.8, 203.5, 210.7, 218.1, 225.7, 233.6, 241.8, 250.3,
];

/// Rate the sub-audible band is decimated to before analysis.
///
/// The demodulator's sub-audible path is already stopped by 260 Hz, so 1 kHz is
/// comfortably above Nyquist for it and decimation needs no further filtering.
pub const ANALYSIS_RATE: f64 = 1000.0;

/// Samples per detection at [`ANALYSIS_RATE`] — just over one second.
///
/// Resolution is `ANALYSIS_RATE / WINDOW` ≈ 1 Hz. That is set by the closest
/// pair in the standard list, 71.9 and 74.4 Hz: a shorter window cannot tell
/// them apart, and reporting the wrong agency is worse than reporting none.
pub const WINDOW: usize = 1024;

/// A configured receiver only has to confirm one known tone, so it can open
/// sooner than a scanner trying to distinguish every possible tone.
const EXPECTED_WINDOW: usize = 512;

/// How far the strongest tone must stand above the average of the rest.
const MIN_RATIO: f32 = 4.0;

/// Absolute magnitude floor, relative to the analysed block's RMS. Stops a
/// silent input from "detecting" whichever tone noise happened to favour.
const MIN_RELATIVE: f32 = 0.15;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Detection {
    pub tone_hz: f32,
    /// Strongest tone's magnitude divided by the mean of the others.
    pub confidence: f32,
}

/// Magnitude of `freq` in `x`, by the Goertzel recurrence.
pub fn goertzel(x: &[f32], fs: f64, freq: f32) -> f32 {
    let w = std::f32::consts::TAU * freq / fs as f32;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for &v in x {
        let s0 = v + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0).sqrt() / x.len() as f32
}

pub struct CtcssDetector {
    decim: usize,
    counter: usize,
    buf: Vec<f32>,
}

impl CtcssDetector {
    pub fn new(fs_in: f64) -> Self {
        Self {
            decim: ((fs_in / ANALYSIS_RATE).round() as usize).max(1),
            counter: 0,
            buf: Vec::with_capacity(WINDOW),
        }
    }

    /// Feed the demodulator's sub-audible output. Returns a verdict once a full
    /// window has accumulated — `Some(None)` meaning "analysed, no tone".
    pub fn push(&mut self, subaudible: &[f32]) -> Option<Option<Detection>> {
        self.accumulate(subaudible);
        if self.buf.len() < WINDOW {
            return None;
        }
        let verdict = analyse(&self.buf[..WINDOW]);
        self.buf.drain(..WINDOW);
        Some(verdict)
    }

    /// Confirm one configured tone with a shorter acquisition window. Unknown
    /// tone discovery still uses [`push`](Self::push) and its finer resolution.
    pub fn push_expected(
        &mut self,
        subaudible: &[f32],
        expected_hz: f32,
    ) -> Option<Option<Detection>> {
        self.accumulate(subaudible);
        if self.buf.len() < EXPECTED_WINDOW {
            return None;
        }
        let verdict = analyse(&self.buf[..EXPECTED_WINDOW])
            .filter(|detection| (detection.tone_hz - expected_hz).abs() < 0.2);
        self.buf.drain(..EXPECTED_WINDOW);
        Some(verdict)
    }

    fn accumulate(&mut self, subaudible: &[f32]) {
        for &v in subaudible {
            if self.counter == 0 {
                self.buf.push(v);
            }
            self.counter = (self.counter + 1) % self.decim;
        }
    }

    pub fn reset(&mut self) {
        self.buf.clear();
        self.counter = 0;
    }
}

/// Score every standard tone over one window and pick a winner, if there is one.
pub fn analyse(block: &[f32]) -> Option<Detection> {
    if block.is_empty() {
        return None;
    }
    let mags: Vec<f32> = TONES
        .iter()
        .map(|&f| goertzel(block, ANALYSIS_RATE, f))
        .collect();
    let (best, &peak) = mags
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .expect("TONES is not empty");
    let others: f32 = (mags.iter().sum::<f32>() - peak) / (mags.len() - 1) as f32;
    let ratio = peak / others.max(f32::MIN_POSITIVE);

    let rms = (block.iter().map(|v| v * v).sum::<f32>() / block.len() as f32).sqrt();
    if ratio < MIN_RATIO || peak < rms * MIN_RELATIVE {
        return None;
    }
    Some(Detection {
        tone_hz: TONES[best],
        confidence: ratio,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn tone(n: usize, fs: f64, freq: f32, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|i| amp * (TAU * freq * i as f32 / fs as f32).sin())
            .collect()
    }

    fn noise(n: usize, seed: u32) -> Vec<f32> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                (s >> 8) as f32 / 8388608.0 - 1.0
            })
            .collect()
    }

    #[test]
    fn the_litchfield_ems_tone_is_identified() {
        let d = analyse(&tone(WINDOW, ANALYSIS_RATE, 192.8, 0.3)).expect("should detect");
        assert_eq!(d.tone_hz, 192.8);
        assert!(d.confidence > MIN_RATIO);
    }

    /// Every standard tone must resolve to itself, including the crowded low
    /// end where 71.9 and 74.4 are only 2.5 Hz apart.
    #[test]
    fn every_standard_tone_resolves_to_itself() {
        for &f in &TONES {
            let d = analyse(&tone(WINDOW, ANALYSIS_RATE, f, 0.3))
                .unwrap_or_else(|| panic!("{f} Hz was not detected"));
            assert_eq!(d.tone_hz, f, "{f} Hz was reported as {}", d.tone_hz);
        }
    }

    #[test]
    fn noise_alone_reports_no_tone() {
        assert!(analyse(&noise(WINDOW, 42)).is_none());
    }

    #[test]
    fn silence_reports_no_tone() {
        assert!(analyse(&vec![0.0; WINDOW]).is_none());
    }

    /// A carrier with no CTCSS must not be given one, or every agency on a
    /// shared channel becomes whichever tone the noise favoured.
    #[test]
    fn a_tone_off_the_standard_grid_is_not_forced_onto_it() {
        // 160 Hz sits between 156.7 and 162.2; neither should win convincingly.
        let verdict = analyse(&tone(WINDOW, ANALYSIS_RATE, 160.0, 0.3));
        if let Some(d) = verdict {
            assert!(
                d.confidence < 12.0,
                "an off-grid tone was reported as {} with confidence {:.1}",
                d.tone_hz,
                d.confidence
            );
        }
    }

    #[test]
    fn the_detector_decimates_and_emits_once_per_window() {
        let fs = 16_000.0;
        let mut det = CtcssDetector::new(fs);
        let input = tone(WINDOW * 16, fs, 100.0, 0.3);
        // One window's worth of decimated samples needs WINDOW*decim inputs.
        let verdict = det
            .push(&input)
            .expect("a full window should have analysed");
        assert_eq!(verdict.map(|d| d.tone_hz), Some(100.0));
    }

    #[test]
    fn a_partial_window_yields_nothing_yet() {
        let mut det = CtcssDetector::new(16_000.0);
        assert!(det.push(&tone(1000, 16_000.0, 100.0, 0.3)).is_none());
    }

    #[test]
    fn a_configured_tone_is_confirmed_in_half_a_second() {
        let mut det = CtcssDetector::new(16_000.0);
        let input = tone(EXPECTED_WINDOW * 16, 16_000.0, 82.5, 0.3);
        let verdict = det
            .push_expected(&input, 82.5)
            .expect("configured window should have analysed");
        assert_eq!(verdict.map(|d| d.tone_hz), Some(82.5));
    }
}
