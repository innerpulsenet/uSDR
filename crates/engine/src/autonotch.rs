//! Adaptive audio auto-notch for the monitor path.
//!
//! A stable interfering tone sitting on top of a weak signal is the classic
//! reason a marginal carrier becomes unreadable. This tracks the strongest
//! persistent narrowband components in 400–2800 Hz and nulls each with a
//! biquad whose depth follows how consistently the tone has been present.
//! "Persistent" is the operative filter: speech energy moves constantly and
//! never earns a notch, while a heterodyne sits on one frequency for seconds.
//! Persistence is judged in frequency as well as time, so the scan resolves
//! a tone to a few hertz and a candidate must be re-found within
//! [`MATCH_HZ`] of where it was.

use std::f32::consts::{PI, TAU};

/// One tracked candidate tone, and the notch serving it once it has earned
/// one. The notch lives with its candidate so its biquad state carries
/// across rescans: rebuilding the filters from zero state on every scan let
/// the tone burst through for a millisecond each time, a 23 Hz buzz.
struct Candidate {
    freq_hz: f32,
    /// Smoothed dominance score; the notch depth follows it directly.
    score: f32,
    /// Consecutive scans that have found this tone. A notch needs
    /// [`CONFIRM_SCANS`] in a row: a held vowel can park a harmonic for a
    /// scan or two, a heterodyne is there every time.
    hits: u32,
    /// Found by the current scan.
    seen: bool,
    notch: Option<Notch>,
}

/// A placed biquad notch with its own state.
struct Notch {
    /// `(1 + r²)/2`: scales the zeros so the passband away from the notch
    /// sits at unity gain instead of up to +0.3 dB.
    b0: f32,
    /// `2·cos(w0)` — the feedforward coefficient, cached so the per-sample
    /// work is multiplies only.
    c2: f32,
    /// `2·r·cos(w0)` — the feedback coefficient.
    pc2: f32,
    /// `r²`.
    r2: f32,
    /// Blend weight now, slewing toward `target` by `slew` per sample so a
    /// depth change between scans is a ramp, not a step in the tone level.
    depth: f32,
    target: f32,
    slew: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Notch {
    fn new(freq_hz: f32, fs: f32) -> Self {
        let mut n = Self {
            b0: 1.0,
            c2: 0.0,
            pc2: 0.0,
            r2: 0.0,
            depth: 0.0,
            target: 0.0,
            slew: 1.0 / (DEPTH_RAMP_S * fs).max(1.0),
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        };
        n.retune(freq_hz, fs);
        n
    }

    /// Moves the notch without touching its state, so a few hertz of
    /// refinement between scans does not restart the filter.
    fn retune(&mut self, freq_hz: f32, fs: f32) {
        // Pole radius from the -3 dB width, so the notch is equally narrow
        // at any monitor rate.
        let r = (-PI * NOTCH_BW_HZ / fs).exp();
        let c = (TAU * freq_hz / fs).cos();
        self.b0 = 0.5 * (1.0 + r * r);
        self.c2 = 2.0 * c;
        self.pc2 = 2.0 * r * c;
        self.r2 = r * r;
    }

    fn process(&mut self, buf: &mut [f32]) {
        for s in buf.iter_mut() {
            let x = *s;
            // Zeroes at exp(±jw0): 1 - 2cos(w0) z^-1 + z^-2
            // Poles at r·exp(±jw0): denominator (1 - 2r·cos(w0) z^-1 + r² z^-2)
            let ff = self.b0 * (x - self.c2 * self.x1 + self.x2);
            let y = ff + self.pc2 * self.y1 - self.r2 * self.y2;
            self.x2 = self.x1;
            self.x1 = x;
            self.y2 = self.y1;
            self.y1 = y;
            self.depth += (self.target - self.depth).clamp(-self.slew, self.slew);
            // Blend by depth: depth 1 = fully nulled output. The target is
            // capped at 1.0 where it is set: the blend is linear in depth,
            // so a depth past unity does not null "harder", it INVERTS the
            // phase of the tone at the output and re-injects it as a louder
            // artifact.
            *s = x * (1.0 - self.depth) + y * self.depth;
        }
    }
}

pub struct AutoNotch {
    fs: f32,
    candidates: Vec<Candidate>,
    sample_counter: usize,
    /// Rolling window for the periodic spectral scan.
    window: Vec<f32>,
    /// Hann taper over the window, so a strong tone's sidelobes do not
    /// read as peaks of their own.
    taper: Vec<f32>,
    /// Scan grid: first frequency, spacing and count.
    grid_lo: f32,
    grid_step: f32,
    grid_n: usize,
    /// Scratch for the tapered window and the per-bin powers.
    tapered: Vec<f32>,
    power: Vec<f32>,
}

const SCAN_INTERVAL: usize = 2048;
const WINDOW_LEN: usize = 1024;
const MIN_HZ: f32 = 400.0;
const MAX_HZ: f32 = 2_800.0;
/// Most scan frequencies per rescan; bounds the Goertzel work at low rates.
const MAX_BINS: usize = 128;
/// Share of in-band energy a peak must hold to count as a candidate.
const PEAK_SHARE: f32 = 0.12;
/// Score above which a candidate earns a full notch.
const OPEN_SCORE: f32 = 0.6;
/// Below this the notch releases.
const CLOSE_SCORE: f32 = 0.25;
const MAX_NOTCHES: usize = 3;
/// How far a peak may sit from a candidate and still be the same tone. A
/// heterodyne holds its frequency to a hertz or two; a voice harmonic glides
/// tens of hertz between scans, so it keeps starting new candidates instead
/// of building one up.
const MATCH_HZ: f32 = 8.0;
/// Re-finds needed before a candidate gets a notch (about 130 ms).
const CONFIRM_SCANS: u32 = 4;
/// -3 dB width of each notch. Narrow enough to leave the voice around a
/// heterodyne alone, wide enough that the few-hertz error of the scan's
/// frequency estimate still sits deep in the null.
const NOTCH_BW_HZ: f32 = 100.0;
/// Time for a notch's depth to swing fully in or out.
const DEPTH_RAMP_S: f32 = 0.010;

impl AutoNotch {
    pub fn new(fs_hz: f64) -> Self {
        let fs = fs_hz as f32;
        let hi = MAX_HZ.min(0.45 * fs);
        // Half a DFT bin of the window apart, so a tone between grid
        // frequencies still lands near a peak of the Hann response (a
        // coarser grid with a rectangular window has nulls between points).
        let step = (fs / WINDOW_LEN as f32 / 2.0).max((hi - MIN_HZ) / MAX_BINS as f32);
        let grid_n = (((hi - MIN_HZ) / step).floor() as usize + 1).max(3);
        let taper = (0..WINDOW_LEN)
            .map(|i| 0.5 - 0.5 * (TAU * i as f32 / WINDOW_LEN as f32).cos())
            .collect();
        Self {
            fs,
            candidates: Vec::new(),
            sample_counter: 0,
            window: Vec::with_capacity(WINDOW_LEN + SCAN_INTERVAL),
            taper,
            grid_lo: MIN_HZ,
            grid_step: step,
            grid_n,
            tapered: vec![0.0; WINDOW_LEN],
            power: vec![0.0; grid_n],
        }
    }

    pub fn active(&self) -> bool {
        self.candidates
            .iter()
            .any(|c| c.notch.as_ref().is_some_and(|n| n.depth > 0.0 || n.target > 0.0))
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
            if boundary_crossed && self.window.len() == WINDOW_LEN {
                self.rescan();
            }
        }
        // The notches are a cascade of causal filters, so running each over
        // the whole block in turn is the same as interleaving them per sample.
        for c in &mut self.candidates {
            if let Some(n) = &mut c.notch {
                n.process(x);
            }
        }
    }

    /// Goertzel scan of the analysis window; update candidate scores and
    /// re-target the notches.
    fn rescan(&mut self) {
        for ((t, &s), &w) in self.tapered.iter_mut().zip(&self.window).zip(&self.taper) {
            *t = s * w;
        }
        for b in 0..self.grid_n {
            // Exact-frequency Goertzel: the coefficient is 2cos(2πf/fs),
            // no integer-bin rounding needed.
            let hz = self.grid_lo + b as f32 * self.grid_step;
            let coeff = 2.0 * (TAU * hz / self.fs).cos();
            let (mut q1, mut q2) = (0.0f32, 0.0f32);
            for &s in &self.tapered {
                let q0 = s + coeff * q1 - q2;
                q2 = q1;
                q1 = q0;
            }
            self.power[b] = (q1 * q1 + q2 * q2 - coeff * q1 * q2).max(0.0);
        }
        let power = &self.power;
        let total: f32 = power.iter().sum::<f32>().max(1e-12);
        // A tone's Hann main lobe spans ±2 DFT bins; summing ±1.5 of them
        // around a peak gathers nearly all of a pure tone's energy, so its
        // share reads near 1 wherever it falls on the grid.
        let lobe = ((1.5 * self.fs / WINDOW_LEN as f32) / self.grid_step).round() as usize;

        let mut found: Vec<(f32, f32)> = Vec::new();
        for b in 1..self.grid_n - 1 {
            if !(power[b] > power[b - 1] && power[b] >= power[b + 1]) {
                continue;
            }
            let lobe_power: f32 =
                power[b.saturating_sub(lobe)..=(b + lobe).min(self.grid_n - 1)].iter().sum();
            let share = lobe_power / total;
            if share > PEAK_SHARE {
                // Parabolic interpolation on log power puts the estimate
                // within a few hertz of the tone, well inside the notch.
                let (a, m, c) = (
                    power[b - 1].max(1e-30).ln(),
                    power[b].max(1e-30).ln(),
                    power[b + 1].max(1e-30).ln(),
                );
                let den = a - 2.0 * m + c;
                let delta = if den < 0.0 { (0.5 * (a - c) / den).clamp(-0.5, 0.5) } else { 0.0 };
                let hz = self.grid_lo + (b as f32 + delta) * self.grid_step;
                found.push((hz, share.min(1.0)));
            }
        }
        found.sort_by(|a, b| b.1.total_cmp(&a.1));

        // Decay every candidate; reinforce those re-found this scan.
        for c in &mut self.candidates {
            c.score *= 0.6;
            c.seen = false;
        }
        for &(hz, share) in found.iter().take(MAX_NOTCHES) {
            let nearest = self
                .candidates
                .iter_mut()
                .filter(|c| (c.freq_hz - hz).abs() < MATCH_HZ)
                .min_by(|a, b| (a.freq_hz - hz).abs().total_cmp(&(b.freq_hz - hz).abs()));
            match nearest {
                Some(c) => {
                    c.freq_hz += 0.3 * (hz - c.freq_hz);
                    // Clamp the reinforcement at 1.0. The score feeds the
                    // notch depth directly, and a depth past 1.0 inverts the
                    // notch's blend — see `Notch::process`.
                    c.score = (c.score + share * 0.8).min(1.0);
                    c.hits += 1;
                    c.seen = true;
                }
                None => self.candidates.push(Candidate {
                    freq_hz: hz,
                    score: share,
                    hits: 1,
                    seen: true,
                    notch: None,
                }),
            }
        }
        // Confirmation counts unbroken runs only. Once a notch is placed a
        // missed scan (the tone buried under a loud syllable) just lets the
        // score decay, so the notch does not drop out mid-tone.
        for c in &mut self.candidates {
            if !c.seen && c.notch.is_none() {
                c.hits = 0;
            }
        }
        // Keep a candidate while its notch is still ramping out, so the
        // release is a ramp and not a cut.
        self.candidates.retain(|c| {
            c.score > 0.05 || c.notch.as_ref().is_some_and(|n| n.depth > 0.0)
        });

        // Re-target the notches of the strongest confirmed candidates; the
        // filters themselves, and their state, stay where they are.
        self.candidates.sort_by(|a, b| b.score.total_cmp(&a.score));
        let fs = self.fs;
        for (i, c) in self.candidates.iter_mut().enumerate() {
            let target = if i < MAX_NOTCHES && (c.hits >= CONFIRM_SCANS || c.notch.is_some())
                && c.score > CLOSE_SCORE {
                ((c.score - CLOSE_SCORE) / (OPEN_SCORE - CLOSE_SCORE)).clamp(0.0, 1.0)
            } else {
                0.0
            };
            match &mut c.notch {
                Some(n) => {
                    n.retune(c.freq_hz, fs);
                    n.target = target;
                    if target == 0.0 && n.depth == 0.0 {
                        c.notch = None;
                    }
                }
                None if target > 0.0 => {
                    let mut n = Notch::new(c.freq_hz, fs);
                    n.target = target;
                    c.notch = Some(n);
                }
                None => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 48_000.0;

    /// Speech-like test signal: a gliding glottal harmonic series under two
    /// formants, gated by a 4 Hz syllable envelope. Its harmonics move the way
    /// a voice's do, which is exactly what must not earn a notch.
    fn speech(n: usize, seed: u32, f0_hz: f32) -> Vec<f32> {
        let fs = FS as f32;
        let mut out = Vec::with_capacity(n);
        let mut ph = 0.0f32;
        let mut rng = seed;
        for i in 0..n {
            let t = i as f32 / fs;
            let f0 = f0_hz * (1.0 + 0.18 * (TAU * 1.3 * t).sin() + 0.07 * (TAU * 3.1 * t).sin());
            ph += TAU * f0 / fs;
            let mut v = 0.0f32;
            for h in 1..=24 {
                let hz = f0 * h as f32;
                let formant = (-((hz - 700.0) / 300.0).powi(2)).exp()
                    + 0.6 * (-((hz - 1_800.0) / 400.0).powi(2)).exp()
                    + 0.05;
                v += formant * (ph * h as f32).sin() / h as f32;
            }
            let syl = (0.5 - 0.5 * (TAU * 4.0 * t).cos()).powi(2);
            rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (rng >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
            out.push(0.35 * syl * v + 0.01 * noise);
        }
        out
    }

    fn tone(n: usize, hz: f32, amp: f32) -> Vec<f32> {
        let w = TAU as f64 * hz as f64 / FS;
        (0..n).map(|i| amp * (w * i as f64).sin() as f32).collect()
    }

    /// Feeds `x` through the notch in 480-sample blocks (10 ms, the
    /// monitor's block size), returning the output.
    fn run(notch: &mut AutoNotch, x: &[f32]) -> Vec<f32> {
        let mut out = x.to_vec();
        for c in out.chunks_mut(480) {
            notch.process(c);
        }
        out
    }

    /// Amplitude of the `hz` component of `x`, by single-bin DFT.
    fn amp_at(x: &[f32], hz: f32) -> f32 {
        let w = TAU as f64 * hz as f64 / FS;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, &s) in x.iter().enumerate() {
            re += s as f64 * (w * i as f64).cos();
            im += s as f64 * (w * i as f64).sin();
        }
        (2.0 * (re * re + im * im).sqrt() / x.len() as f64) as f32
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    /// A heterodyne between the scan's grid points, under speech, is pulled
    /// down hard once the notch settles. 656.25 Hz sat exactly between the
    /// old 187.5 Hz grid points, where a 1024-sample Goertzel has a null.
    #[test]
    fn off_grid_heterodyne_under_speech_is_nulled() {
        for &hz in &[656.25f32, 1_030.0, 2_217.0] {
            let n = (FS * 4.0) as usize;
            let sp = speech(n, 7, 135.0);
            let tn = tone(n, hz, 0.2);
            let x: Vec<f32> = sp.iter().zip(&tn).map(|(a, b)| a + b).collect();
            let mut notch = AutoNotch::new(FS);
            let y = run(&mut notch, &x);
            let tail = n - (FS * 1.0) as usize;
            let before = amp_at(&x[tail..], hz);
            let after = amp_at(&y[tail..], hz);
            let db = 20.0 * (after / before).log10();
            eprintln!("{hz} Hz heterodyne: {db:.1} dB");
            assert!(db < -20.0, "{hz} Hz only reduced {db:.1} dB");
        }
    }

    /// Once a notch holds a steady tone, rescans must not disturb it: the old
    /// rescan rebuilt every biquad from zero state every 2048 samples, and
    /// the tone burst through for a millisecond each time, a 23 Hz buzz.
    #[test]
    fn rescans_do_not_click() {
        let n = (FS * 4.0) as usize;
        let x = tone(n, 1_000.0, 0.5);
        let mut notch = AutoNotch::new(FS);
        let y = run(&mut notch, &x);
        let tail = &y[n - (FS * 1.0) as usize..];
        let peak = tail.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        let peak_db = 20.0 * (peak / 0.5).log10();
        // Short-window envelope: worst 1 ms window against the median one.
        let mut wins: Vec<f32> = tail.chunks(48).map(rms).collect();
        let worst = wins.iter().cloned().fold(0.0f32, f32::max);
        wins.sort_by(f32::total_cmp);
        let median = wins[wins.len() / 2].max(1e-9);
        eprintln!(
            "steady tone residual peak {peak_db:.1} dB re tone, worst/median 1 ms window {:.1} dB",
            20.0 * (worst / median).log10()
        );
        assert!(peak_db < -30.0, "residual peaks at {peak_db:.1} dB re tone");
    }

    /// Speech alone never earns a notch deep enough to hear: the output
    /// stays within 25 dB of the input (a harmonic parked at the turn of a
    /// pitch glide can still earn a brief shallow one). The old detector,
    /// with its 80 Hz match window and 458 Hz wide notches, left the error
    /// 2.5 dB under the voice.
    #[test]
    fn speech_alone_passes_untouched() {
        for &f0 in &[110.0f32, 135.0, 210.0] {
            let n = (FS * 4.0) as usize;
            let x = speech(n, 11, f0);
            let mut notch = AutoNotch::new(FS);
            let y = run(&mut notch, &x);
            let err: Vec<f32> = x.iter().zip(&y).map(|(a, b)| a - b).collect();
            let snr = 20.0 * (rms(&x) / rms(&err).max(1e-12)).log10();
            eprintln!("{f0} Hz voice through the notch: error {snr:.1} dB below signal");
            assert!(snr > 25.0, "{f0} Hz voice: error only {snr:.1} dB below the signal");
        }
    }

    /// When the heterodyne goes away its notch ramps out and is dropped.
    #[test]
    fn notch_releases_after_the_tone_stops() {
        let n = (FS * 2.0) as usize;
        let mut notch = AutoNotch::new(FS);
        run(&mut notch, &tone(n, 1_400.0, 0.3));
        assert!(notch.active(), "never notched a steady tone");
        run(&mut notch, &speech(n, 3, 135.0));
        assert!(!notch.active(), "notch still in after two seconds without the tone");
    }

    /// Low monitor rates still get a sane scan grid.
    #[test]
    fn low_rates_build() {
        for fs in [8_000.0, 12_000.0, 24_000.0] {
            let mut notch = AutoNotch::new(fs);
            let mut x: Vec<f32> = (0..2 * fs as usize)
                .map(|i| (TAU * 1_000.0 * i as f32 / fs as f32).sin())
                .collect();
            for c in x.chunks_mut(480) {
                notch.process(c);
            }
            assert!(notch.active(), "no notch at {fs} Hz");
        }
    }
}
