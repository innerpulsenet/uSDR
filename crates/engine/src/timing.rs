//! Symbol-clock tracking for the fixed-phase lane decoders.
//!
//! POCSAG, FLEX and Bell 202 are all decoded here by a bank of slicers at
//! staggered fixed phases: lane `d` starts `d/N` of a symbol into the stream
//! and counts `fs/baud` samples per symbol from there. That covers an unknown
//! *starting* phase, and nothing else. With 16 lanes each one owns 1/16 of a
//! symbol, so a lane survives a frame only while the accumulated timing error
//! stays inside roughly ±1/32 of a symbol — a **57 ppm** budget across a
//! 544-bit POCSAG batch.
//!
//! Nothing lives inside 57 ppm. A transmitter's baud rate is good to maybe
//! 50–100 ppm on its own, and the receive chain contributes whatever
//! `fs_in / round(fs_in / target)` happens to land on. So the bank does not
//! degrade gracefully as the error grows: it decodes, then it decodes the
//! first part of a batch and hands the FEC garbage for the rest.
//!
//! The fix is what every real modem does — measure the timing error and
//! correct it. This is the loop filter for that measurement; the slicers
//! supply the error, because how you measure it depends on what you sliced.
//!
//! It is a proportional-integral loop, but the two halves are not weighted
//! the way a textbook would. The proportional term does most of the work:
//! it steers the sampling phase and cannot accumulate, so noise moves it
//! around without consequence. The integral term learns a genuine rate
//! offset, which is worth having across a long frame — but it *does*
//! accumulate, and on an idle channel there is nothing to accumulate except
//! noise. So it is clamped hard. [`MAX_PULL_PPM`] is where that trade is
//! explained; getting it wrong is not a tuning matter, it is the difference
//! between decoding and silence.

/// How far the integrator may pull the symbol rate, in ppm.
///
/// This bound is the load-bearing part of the design, not a safety margin.
/// A timing detector fed receiver noise returns a *zero-mean* error, and an
/// unbounded integrator turns that into a random walk: measured on real-scale
/// discriminator noise, a loop clamped at 30 000 ppm wandered 14 000 ppm off
/// nominal within a quarter second and stayed pinned near its limit. Since a
/// channel is idle almost all the time, every burst then arrived at a lane
/// running at the wrong rate and nothing decoded at all — strictly worse than
/// the fixed clock it replaced.
///
/// 800 ppm is chosen from the other side: transmitter crystals are good to
/// 50-100 ppm, so this is several times what any real sender needs, while
/// being small enough that a lane which has been staring at noise is still
/// close enough to nominal to acquire the next burst. The proportional term
/// covers everything beyond it, carrying a steady-state phase lag of
/// `error / kp` — 0.01 of a symbol at 1000 ppm — rather than driving the
/// error to zero.
const MAX_PULL_PPM: f64 = 800.0;

/// Tracks the sampling instant for one symbol stream.
#[derive(Clone, Debug)]
pub struct TimingLoop {
    /// Samples per symbol as designed, before any correction.
    nominal: f64,
    /// Current estimate, which the integral term moves.
    period: f64,
    /// Samples accumulated toward the next symbol.
    elapsed: f64,
    kp: f64,
    ki: f64,
    /// How far the period may be pulled from nominal, as a fraction.
    max_pull: f64,
}

impl TimingLoop {
    /// `samples_per_symbol` is `fs / baud`.
    pub fn new(samples_per_symbol: f64) -> Self {
        Self::with_bandwidth(samples_per_symbol, 0.10, MAX_PULL_PPM)
    }

    /// `loop_bw` is the proportional gain in symbols of correction per symbol
    /// of measured error. `max_pull_ppm` of zero makes this a purely
    /// first-order, phase-only loop; see [`MAX_PULL_PPM`].
    pub fn with_bandwidth(samples_per_symbol: f64, loop_bw: f64, max_pull_ppm: f64) -> Self {
        let period = samples_per_symbol.max(2.0);
        Self {
            nominal: period,
            period,
            elapsed: 0.0,
            kp: loop_bw,
            // A decade below the proportional term, when it is used at all.
            ki: if max_pull_ppm > 0.0 {
                loop_bw * 0.02
            } else {
                0.0
            },
            max_pull: max_pull_ppm * 1e-6,
        }
    }

    pub fn reset(&mut self) {
        self.period = self.nominal;
        self.elapsed = 0.0;
    }

    /// Start `delay` samples into the stream — how a lane claims its phase.
    pub fn stagger(&mut self, delay: f64) {
        self.elapsed = -delay;
    }

    pub fn period(&self) -> f64 {
        self.period
    }

    /// Parts per million the loop has pulled the clock. Diagnostics: a value
    /// pinned at the limit means the chain rate is wrong, not the transmitter.
    pub fn pull_ppm(&self) -> f64 {
        (self.period / self.nominal - 1.0) * 1e6
    }

    /// Re-target the loop, keeping the phase — FLEX changes symbol rate
    /// mid-stream once the sync word says which mode this frame is.
    pub fn set_period(&mut self, samples_per_symbol: f64) {
        self.nominal = samples_per_symbol.max(2.0);
        self.period = self.nominal;
        self.elapsed = 0.0;
    }

    /// Whether the *next* sample falls in the back half of the current
    /// symbol. Lets an integrate-and-dump slicer split its accumulation by
    /// the clock's real position rather than by a sample count that goes
    /// stale as soon as the period is pulled.
    pub fn at_second_half(&self) -> bool {
        self.elapsed >= self.period * 0.5
    }

    /// Position within the current symbol, 0..1. Exposed so sibling lanes in
    /// a decoder bank can combine their evidence at matched phases: two lanes
    /// whose clocks agree within ~15% are seeing the same bits and may vote.
    pub fn phase_fraction(&self) -> f64 {
        (self.elapsed / self.period).clamp(0.0, 1.0)
    }

    /// Advance one input sample. `true` means a symbol boundary was reached
    /// and the caller should slice now.
    pub fn tick(&mut self) -> bool {
        self.elapsed += 1.0;
        if self.elapsed < self.period {
            return false;
        }
        self.elapsed -= self.period;
        true
    }

    /// Feed a timing error, in symbols. **Positive means sampling late** —
    /// the true symbol boundary was earlier than where we sliced.
    ///
    /// The error is clamped before it is applied: a detector fed noise can
    /// report anything, and one wild sample must not cost the lock that the
    /// last hundred good ones bought.
    ///
    /// The integral term is deliberately kept on a short leash — see
    /// [`MAX_PULL_PPM`] for why that bound is the difference between a loop
    /// that works on air and one that decodes nothing at all.
    pub fn correct(&mut self, error_symbols: f64) {
        if !error_symbols.is_finite() {
            return;
        }
        let e = error_symbols.clamp(-0.5, 0.5);
        // Proportional: bring the next sampling instant forward. `tick` fires
        // when `elapsed` reaches `period`, so adding to `elapsed` samples
        // sooner — which is what a *late* measurement asks for.
        self.elapsed += self.kp * e * self.period;
        // Integral: sampling drifts late when our period is longer than the
        // transmitter's, so a persistent late error means shorten it. The
        // sign here is the whole value of the integrator — get it backwards
        // and it is positive feedback that destroys a lock instead of holding
        // one.
        self.period -= self.ki * e * self.period;
        let lo = self.nominal * (1.0 - self.max_pull);
        let hi = self.nominal * (1.0 + self.max_pull);
        self.period = self.period.clamp(lo, hi);
    }

    /// Ease the period back toward nominal. Call this instead of `correct`
    /// when the caller knows there is no signal to steer on, so an idle lane
    /// converges on nominal rather than on whatever noise last suggested.
    pub fn relax(&mut self) {
        const PULL_BACK: f64 = 0.05;
        self.period += PULL_BACK * (self.nominal - self.period);
    }

    /// Forget the tracked rate outright and go back to nominal.
    ///
    /// For when the caller knows the thing it was tracking has ended — an
    /// HDLC frame aborted, a batch lost sync. Whatever the loop learned
    /// belonged to that burst, and carrying it into the hunt for the next one
    /// is how a lane ends up unable to find anything.
    pub fn reacquire(&mut self) {
        self.period = self.nominal;
    }
}

/// Gardner timing-error detector for an integrate-and-dump slicer.
///
/// Gardner's detector needs the symbol values either side of a boundary and
/// one sample from the middle. Integrating each symbol in two halves gives
/// all three for free: the halves average to the symbol, and the two halves
/// straddling a boundary average to the midpoint. It is decision-directed
/// only through the difference of successive symbols, so it works unchanged
/// on two-level POCSAG and four-level FLEX, and it reports nothing when there
/// is no transition — which is correct, because a run of equal symbols
/// carries no timing information.
#[derive(Clone, Copy, Debug, Default)]
pub struct GardnerTed {
    prev_first: f32,
    prev_second: f32,
    prev_symbol: f32,
    /// Running magnitude, so loop gain does not scale with signal level.
    scale: f32,
    primed: u8,
}

impl GardnerTed {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Offer the two half-symbol averages of the symbol just sliced.
    /// Returns the timing error in symbols, positive when sampling late.
    pub fn push(&mut self, first_half: f32, second_half: f32) -> Option<f64> {
        let symbol = 0.5 * (first_half + second_half);
        let midpoint = 0.5 * (self.prev_second + first_half);
        let out = if self.primed >= 2 {
            self.scale += 0.02 * (symbol.abs() - self.scale);
            let norm = (self.scale * self.scale).max(1e-9);
            // (y[k] - y[k-1]) * y[k-1/2], normalised to a symbol fraction.
            // Positive product means the midpoint sits on the *new* symbol's
            // side of where we think the boundary is, i.e. we sliced late.
            Some(f64::from((symbol - self.prev_symbol) * midpoint / norm) * 0.25)
        } else {
            self.primed += 1;
            self.scale = symbol.abs().max(1e-6);
            None
        };
        self.prev_first = first_half;
        self.prev_second = second_half;
        self.prev_symbol = symbol;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A loop fed a steady "sampling late" error must *shorten* its period:
    /// drifting late means the clock is running slow relative to the sender,
    /// and the integrator is there to learn that offset once rather than
    /// chase it every symbol.
    #[test]
    fn a_steady_error_is_absorbed_into_the_period() {
        let mut t = TimingLoop::new(13.333);
        for _ in 0..2000 {
            t.correct(0.02);
        }
        assert!(
            t.period() < 13.333,
            "period should have shrunk, got {}",
            t.period()
        );
        assert!(t.pull_ppm() < -100.0, "pull {} ppm", t.pull_ppm());

        let mut early = TimingLoop::new(13.333);
        for _ in 0..2000 {
            early.correct(-0.02);
        }
        assert!(early.pull_ppm() > 100.0, "pull {} ppm", early.pull_ppm());
    }

    /// The bound in [`MAX_PULL_PPM`] is what keeps an idle channel from
    /// walking the clock somewhere no burst can be decoded from, so it has to
    /// actually bind — including against a detector reporting garbage.
    #[test]
    fn noise_cannot_walk_the_rate_away() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut rng = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            ((seed >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
        };
        let mut t = TimingLoop::new(13.333);
        for _ in 0..500_000 {
            t.correct(rng());
        }
        assert!(
            t.pull_ppm().abs() <= MAX_PULL_PPM + 0.1,
            "half a million noise samples pulled {:.0} ppm",
            t.pull_ppm()
        );
    }

    /// And the proportional term, which carries everything past that bound,
    /// must not accumulate at all.
    #[test]
    fn a_phase_only_loop_never_moves_its_rate() {
        let mut t = TimingLoop::with_bandwidth(13.333, 0.10, 0.0);
        for _ in 0..10_000 {
            t.correct(0.4);
        }
        assert_eq!(t.pull_ppm(), 0.0);
    }

    #[test]
    fn the_period_cannot_run_away() {
        let mut t = TimingLoop::with_bandwidth(20.0, 0.5, 1_000.0);
        for _ in 0..100_000 {
            t.correct(0.5);
        }
        assert!(t.pull_ppm() >= -1_000.1, "pull {} ppm", t.pull_ppm());
        let mut t = TimingLoop::with_bandwidth(20.0, 0.5, 1_000.0);
        for _ in 0..100_000 {
            t.correct(-0.5);
        }
        assert!(t.pull_ppm() <= 1_000.1, "pull {} ppm", t.pull_ppm());
    }

    #[test]
    fn a_nonsense_error_is_ignored() {
        let mut t = TimingLoop::new(16.0);
        let before = t.period();
        t.correct(f64::NAN);
        t.correct(f64::INFINITY);
        assert_eq!(t.period(), before);
    }

    /// Ticking at the nominal period yields exactly `baud` symbols per second.
    #[test]
    fn an_untouched_loop_ticks_at_the_nominal_rate() {
        let mut t = TimingLoop::new(16_000.0 / 1200.0);
        let ticks = (0..16_000).filter(|_| t.tick()).count();
        assert!((ticks as i32 - 1200).abs() <= 1, "got {ticks} ticks");
    }

    /// Gardner reports nothing without a transition, and opposite signs for
    /// early and late sampling.
    #[test]
    fn gardner_is_quiet_without_a_transition_and_signed_with_one() {
        let mut ted = GardnerTed::new();
        for _ in 0..6 {
            ted.push(1.0, 1.0);
        }
        let flat = ted.push(1.0, 1.0).unwrap();
        assert!(
            flat.abs() < 1e-6,
            "a flat run should carry no timing: {flat}"
        );

        // Sampling late: our window for the new symbol still holds some of the
        // old one, so its first half is contaminated toward the old level.
        let mut late = GardnerTed::new();
        for _ in 0..4 {
            late.push(-1.0, -1.0);
        }
        let e_late = late.push(-0.4, 1.0).unwrap();

        // Sampling early: the *previous* symbol's second half already carries
        // the new level.
        let mut early = GardnerTed::new();
        for _ in 0..3 {
            early.push(-1.0, -1.0);
        }
        early.push(-1.0, -0.4);
        let e_early = early.push(1.0, 1.0).unwrap();

        assert!(
            e_late * e_early < 0.0,
            "early and late must disagree in sign: late {e_late}, early {e_early}"
        );
    }
}
