//! Audio-frequency FSK: Goertzel mark/space over one baud period.
//!
//! The FM discriminator output is a *tone*, not a DC offset. Comparing the
//! instantaneous deviation to 1200 Hz cannot decode Bell 202 or SAME.

use crate::timing::TimingLoop;
use std::f32::consts::PI;

pub struct ToneSlicer {
    coeff_m: f32,
    coeff_s: f32,
    spb: f32,
    /// Samples in the correlation window, and the early/late gate offset.
    win: usize,
    gate: usize,
    clock: TimingLoop,
    /// Whether the caller believes we are inside a frame. The clock only
    /// tracks then; otherwise it eases back to nominal so an idle channel
    /// cannot random-walk it somewhere no burst can be decoded from.
    tracking: bool,
    delay: usize,
    delay0: usize,
    /// Chronological history, `win + 2 * gate` deep: enough to place the
    /// on-time window with a full gate of context either side of it.
    ///
    /// Stored as a ring: `hist_head` is the oldest sample. Shifting a ~50-float
    /// array once per sample per lane (`rotate_left`) was ~150 MB/s of small
    /// memmoves across a 16-lane bank at 48 kHz; indexing the ring is two adds.
    hist: Vec<f32>,
    hist_head: usize,
    filled: usize,
    pub last_pm: f32,
    pub last_ps: f32,
    /// Last data decision, for hysteresis. Seeded to `false` (space) so a
    /// clean first mark claim wins immediately.
    last_bit: bool,
}

/// Samples in bit `k` when the bit clock is `spb` samples long.
pub fn bit_len(k: usize, spb: f32) -> usize {
    let a = (k as f32 * spb).round() as usize;
    let b = ((k as f32 + 1.0) * spb).round() as usize;
    (b - a).max(1)
}

impl ToneSlicer {
    pub fn new(fs: f64, mark_hz: f32, space_hz: f32, baud: f32) -> Self {
        let fsf = fs as f32;
        let spb = (fsf / baud).max(4.0);
        let win = spb.round().max(4.0) as usize;
        // A quarter of a symbol either side is the usual early/late spacing:
        // wide enough that the correlation difference is measurable, narrow
        // enough that both gates stay inside the same pair of symbols.
        let gate = (win / 4).max(1);
        Self {
            coeff_m: 2.0 * (2.0 * PI * mark_hz / fsf).cos(),
            coeff_s: 2.0 * (2.0 * PI * space_hz / fsf).cos(),
            spb,
            win,
            gate,
            clock: TimingLoop::new(f64::from(spb))
                .expect("afsk spb is clamped to at least 4 sps"),
            tracking: false,
            delay: 0,
            delay0: 0,
            hist: vec![0.0; win + 2 * gate],
            hist_head: 0,
            filled: 0,
            last_pm: 0.0,
            last_ps: 0.0,
            last_bit: false,
        }
    }

    /// Skip the first `delay` samples so a bank of slicers can cover one baud.
    pub fn with_delay(mut self, delay: usize) -> Self {
        self.delay = delay;
        self.delay0 = delay;
        self
    }

    pub fn reset(&mut self) {
        self.clock.reset();
        self.tracking = false;
        self.hist.iter_mut().for_each(|x| *x = 0.0);
        self.hist_head = 0;
        self.filled = 0;
        self.delay = self.delay0;
    }

    /// Tell the slicer whether it is inside a frame. See `TimingLoop::relax`.
    pub fn set_tracking(&mut self, tracking: bool) {
        self.tracking = tracking;
    }

    /// Drop the tracked rate and hunt again at nominal, keeping the phase.
    pub fn reacquire(&mut self) {
        self.tracking = false;
        self.clock.reacquire();
    }

    pub fn samples_per_bit(&self) -> usize {
        self.spb.round() as usize
    }

    pub fn samples_per_bit_f(&self) -> f32 {
        self.spb
    }

    /// Parts per million the bit clock has been pulled from nominal.
    pub fn clock_pull_ppm(&self) -> f64 {
        self.clock.pull_ppm()
    }

    /// Correlation over `self.win` samples ending `back` samples before the
    /// newest one. The ring keeps samples chronological under the head.
    fn gate_at(&self, back: usize) -> (f32, f32) {
        let n = self.hist.len();
        // Index of the newest sample; walk back from there over the window.
        let newest = (self.hist_head + n - 1) % n;
        let end = (newest + n - back + n) % n;
        (
            goertzel_ring(&self.hist, end, self.win, self.coeff_m),
            goertzel_ring(&self.hist, end, self.win, self.coeff_s),
        )
    }

    /// `true` = mark tone, `false` = space tone.
    ///
    /// The decision is deliberately one gate behind the newest sample: the
    /// late gate needs samples from *after* the symbol it is judging, and a
    /// gate of delay is cheaper than being unable to measure timing at all.
    pub fn push(&mut self, x: f32) -> Option<bool> {
        if self.delay > 0 {
            self.delay -= 1;
            return None;
        }
        // Ring write: overwrite the oldest slot and advance the head. The old
        // `rotate_left(1)` moved every element of `hist` on every sample.
        let n = self.hist.len();
        self.hist[self.hist_head] = x;
        self.hist_head = (self.hist_head + 1) % n;
        self.filled = self.filled.saturating_add(1);
        if !self.clock.tick() {
            return None;
        }
        if self.filled < self.hist.len() {
            // Not enough history yet to place the window honestly.
            return None;
        }
        // On-time window sits one gate back, so `late` has real samples.
        let (pm, ps) = self.gate_at(self.gate);
        let (em, es) = self.gate_at(2 * self.gate);
        let (lm, ls) = self.gate_at(0);
        self.last_pm = pm;
        self.last_ps = ps;

        // The mark/space discriminant is largest when the window covers one
        // whole symbol; straddling two different symbols splits the energy
        // between both tones and shrinks it. So comparing the early and late
        // gates says which side of the true symbol we are sitting on, and a
        // run of identical symbols correctly reports nothing.
        //
        // Only steer on it while the caller says we are in a frame, and only
        // when this window actually looks like one of the two tones rather
        // than like noise: a 13-sample correlation on noise reports a
        // confident-looking error just as readily as a real one.
        let early = (em - es).abs();
        let late = (lm - ls).abs();
        let denom = early + late;
        let decisive = (pm - ps).abs() > 0.2 * (pm + ps + 1e-9);
        if self.tracking && decisive && denom > 1e-9 {
            let error = f64::from((early - late) / denom) * 0.5;
            self.clock.correct(error);
        } else {
            self.clock.relax();
        }
        // Hysteresis on the data decision, with a deliberately narrow band:
        // a bare `pm > ps` flips twice per symbol that wanders across the
        // threshold, and each flip becomes two NRZI bit errors downstream —
        // but a wide sticky band holds one tone through flag patterns and
        // kills sync entirely. 4% of tone power is enough to stop dithering
        // without ever outranking a real opposite-tone claim.
        let band = 0.04 * (pm + ps + 1e-9);
        let bit = if self.last_bit {
            !(ps - pm > band)
        } else {
            pm - ps > band
        };
        self.last_bit = bit;
        Some(bit)
    }
}

/// Goertzel power over a contiguous slice. Kept for CTCSS's analysis path;
/// the ToneSlicer reads its ring history through [`goertzel_ring`].
#[allow(dead_code)]
fn goertzel(x: &[f32], coeff: f32) -> f32 {
    let mut s1 = 0.0;
    let mut s2 = 0.0;
    let denom = x.len().saturating_sub(1).max(1) as f32;
    for (i, &sample) in x.iter().enumerate() {
        // A baud is not an integer number of cycles for either Bell-202 tone.
        // Windowing keeps the resulting spectral leakage from making the
        // other tone look stronger near a symbol transition.
        let w = 0.5 - 0.5 * (2.0 * PI * i as f32 / denom).cos();
        let v = sample * w;
        let s0 = v + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    s1 * s1 + s2 * s2 - coeff * s1 * s2
}

/// Goertzel power over `n` ring samples ending at (and including) index
/// `end_idx`, oldest first. Same Hann window and recursion as [`goertzel`],
/// just reading the history through the ring order.
///
/// The Hann coefficient depends only on `(i, n)`, so it comes from a cached
/// table: recomputing a cosine per sample per gate was ~4.6 M cos()/s across
/// a 16-lane bank at 48 kHz. The window is contiguous in the ring except for
/// one wrap, so the walk is two linear runs — no per-sample modulo.
fn goertzel_ring(hist: &[f32], end_idx: usize, n: usize, coeff: f32) -> f32 {
    let len = hist.len();
    let win = hann_window(n);
    let mut s1 = 0.0f32;
    let mut s2 = 0.0f32;
    // Oldest sample first: start `n - 1` back from the end index.
    let start = end_idx + len - (n - 1);
    let (first_len, wrapped) = if start / len == (start + n - 1) / len || start % len + n <= len {
        (n, false)
    } else {
        (len - start % len, true)
    };
    let lo = start % len;
    for i in 0..first_len {
        let v = hist[lo + i] * win[i];
        let s0 = v + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    if wrapped {
        for i in first_len..n {
            let v = hist[i - first_len] * win[i];
            let s0 = v + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
    }
    s1 * s1 + s2 * s2 - coeff * s1 * s2
}

/// Cached Hann windows by length, shared by every lane's gate evaluations.
///
/// Window lengths are fixed per decoder (one symbol), so the table fills
/// once and every later call is a lookup.
fn hann_window(n: usize) -> &'static [f32] {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static TABLES: OnceLock<Mutex<HashMap<usize, &'static [f32]>>> = OnceLock::new();
    let tables = TABLES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut tables = tables.lock().expect("hann window table");
    *tables.entry(n).or_insert_with(|| {
        let denom = n.saturating_sub(1).max(1) as f32;
        Box::leak(
            (0..n)
                .map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / denom).cos())
                .collect::<Vec<f32>>()
                .into_boxed_slice(),
        )
    })
}

/// Synthesize one baud of a tone at `hz`, amplitude `amp`.
pub fn tone(out: &mut Vec<f32>, fs: f32, hz: f32, n: usize, amp: f32, phase: &mut f32) {
    let step = 2.0 * PI * hz / fs;
    for _ in 0..n {
        out.push(amp * phase.sin());
        *phase += step;
        if *phase > 2.0 * PI {
            *phase -= 2.0 * PI;
        }
    }
}

/// Mark/space bitstream → audio with a fractional bit clock (`fs/baud`).
pub fn marks_to_audio(
    marks: &[bool],
    fs: f32,
    baud: f32,
    mark_hz: f32,
    space_hz: f32,
    amp: f32,
) -> Vec<f32> {
    let spb = fs / baud;
    let mut out = Vec::new();
    let mut ph = 0.0f32;
    for (k, &m) in marks.iter().enumerate() {
        tone(
            &mut out,
            fs,
            if m { mark_hz } else { space_hz },
            bit_len(k, spb),
            amp,
            &mut ph,
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `n` bits of one tone and return the last decision, if any. The
    /// slicer holds its decision a quarter-symbol back so the late gate has
    /// real samples, and needs about one and a half symbols of history before
    /// it will commit to anything at all.
    fn run_tone(s: &mut ToneSlicer, fs: f32, hz: f32, bits: usize, ph: &mut f32) -> Option<bool> {
        let spb = s.samples_per_bit_f();
        let mut out = None;
        for k in 0..bits {
            let mut buf = Vec::new();
            tone(&mut buf, fs, hz, bit_len(k, spb), 1.0, ph);
            for &x in &buf {
                if let Some(b) = s.push(x) {
                    out = Some(b);
                }
            }
        }
        out
    }

    #[test]
    fn goertzel_picks_the_tone() {
        let fs = 16_000.0;
        let mut s = ToneSlicer::new(fs as f64, 1200.0, 2200.0, 1200.0);
        let mut ph = 0.0f32;
        assert_eq!(run_tone(&mut s, fs, 1200.0, 4, &mut ph), Some(true));
        assert_eq!(run_tone(&mut s, fs, 2200.0, 4, &mut ph), Some(false));
        assert_eq!(run_tone(&mut s, fs, 1200.0, 4, &mut ph), Some(true));
    }

    /// One bit per baud, no more and no less, once the slicer is primed.
    #[test]
    fn the_clock_produces_one_bit_per_baud() {
        let fs = 16_000.0f32;
        let mut s = ToneSlicer::new(f64::from(fs), 1200.0, 2200.0, 1200.0);
        let marks: Vec<bool> = (0..1200).map(|i| i % 3 == 0).collect();
        let audio = marks_to_audio(&marks, fs, 1200.0, 1200.0, 2200.0, 1.0);
        let bits = audio.iter().filter_map(|&x| s.push(x)).count();
        // A couple of symbols go to priming the history.
        assert!(
            (1195..=1200).contains(&bits),
            "expected ~1200 bits from one second, got {bits}"
        );
    }

    /// The gate must steer toward a transmitter whose baud rate is not ours.
    /// A slicer that never moves slips out of its lane within a frame.
    #[test]
    fn the_gate_steers_toward_a_wrong_baud_rate() {
        let fs = 16_000.0f32;
        for ppm in [-3_000i32, 3_000] {
            let mut s = ToneSlicer::new(f64::from(fs), 1200.0, 2200.0, 1200.0);
            s.set_tracking(true);
            // Alternating tones give the gate a transition every symbol.
            let marks: Vec<bool> = (0..600).map(|i| i % 2 == 0).collect();
            // A transmitter `ppm` fast has shorter symbols, so the clock has
            // to shorten its period to match: the pull is the opposite sign.
            let air_baud = 1200.0 * (1.0 + ppm as f32 * 1e-6);
            let audio = marks_to_audio(&marks, fs, air_baud, 1200.0, 2200.0, 1.0);
            for &x in &audio {
                s.push(x);
            }
            let pull = s.clock_pull_ppm();
            assert!(
                pull * f64::from(ppm) < 0.0,
                "at {ppm} ppm the clock pulled {pull:.0} ppm, the wrong way"
            );
        }
    }

    /// And it must not steer at all when nobody has said there is a frame:
    /// an idle lane has to keep the staggered phase it was given, or the bank
    /// collapses onto whatever noise last suggested.
    #[test]
    fn an_untracked_slicer_holds_its_nominal_clock() {
        let fs = 16_000.0f32;
        let mut seed = 0x243F_6A88_85A3_08D3u64;
        let mut rng = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            ((seed >> 11) as f32 / (1u64 << 53) as f32) * 2.0 - 1.0
        };
        let mut s = ToneSlicer::new(f64::from(fs), 1200.0, 2200.0, 1200.0);
        for _ in 0..(fs as usize * 4) {
            s.push(rng() * 3000.0);
        }
        assert_eq!(s.clock_pull_ppm(), 0.0);
    }
}
