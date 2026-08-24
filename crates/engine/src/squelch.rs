//! Squelch: turning a fluctuating SNR into a stable open/closed decision.
//!
//! Two mechanisms, and they solve different problems. **Hysteresis** — a lower
//! threshold to stay open than to open — stops a signal sitting exactly on the
//! threshold from chattering. **Hang** keeps the squelch open through the short
//! gaps inside speech, so one transmission becomes one call rather than a dozen
//! fragments split at every pause.

/// Default SNR to open at, in dB over the local noise. Chosen against the
/// measured behaviour of `detect`: an empty channel reads within about 4 dB of
/// zero, so 8 dB clears noise without burying weak traffic.
pub const DEFAULT_OPEN_DB: f32 = 8.0;
/// Default SNR to stay open at. The gap to `DEFAULT_OPEN_DB` is the hysteresis.
pub const DEFAULT_CLOSE_DB: f32 = 5.0;
/// Default seconds below the close threshold before a call is declared over.
pub const DEFAULT_HANG_S: f32 = 1.5;

#[derive(Clone, Debug)]
pub struct Squelch {
    pub open_db: f32,
    pub close_db: f32,
    pub hang_s: f32,
    open: bool,
    below_s: f32,
}

impl Default for Squelch {
    fn default() -> Self {
        Self::new(DEFAULT_OPEN_DB, DEFAULT_CLOSE_DB, DEFAULT_HANG_S)
    }
}

impl Squelch {
    pub fn new(open_db: f32, close_db: f32, hang_s: f32) -> Self {
        Self {
            open_db,
            // A close threshold above the open threshold would invert the
            // hysteresis and make the squelch chatter, so it is clamped rather
            // than trusted.
            close_db: close_db.min(open_db),
            hang_s: hang_s.max(0.0),
            open: false,
            below_s: 0.0,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Advance by `dt_s` with a new SNR reading. Returns whether the squelch is
    /// open *after* this update.
    pub fn update(&mut self, snr_db: f32, dt_s: f32) -> bool {
        if !self.open {
            if snr_db >= self.open_db {
                self.open = true;
                self.below_s = 0.0;
            }
            return self.open;
        }
        if snr_db >= self.close_db {
            self.below_s = 0.0;
        } else {
            self.below_s += dt_s;
            if self.below_s >= self.hang_s {
                self.open = false;
                self.below_s = 0.0;
            }
        }
        self.open
    }

    /// Seconds spent below the close threshold, for a caller that wants to trim
    /// the hang time off the end of a recording.
    pub fn trailing_silence_s(&self) -> f32 {
        self.below_s
    }

    pub fn reset(&mut self) {
        self.open = false;
        self.below_s = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f32 = 0.05;

    #[test]
    fn it_opens_above_the_threshold_and_starts_closed() {
        let mut s = Squelch::default();
        assert!(!s.is_open());
        assert!(!s.update(5.0, DT));
        assert!(s.update(12.0, DT));
    }

    /// The point of hysteresis: a signal between the two thresholds holds the
    /// squelch open once opened, but could not have opened it.
    #[test]
    fn hysteresis_holds_a_marginal_signal_without_letting_it_open() {
        let mut s = Squelch::default();
        assert!(!s.update(6.5, DT), "6.5 dB is below the open threshold");
        s.update(12.0, DT);
        for _ in 0..100 {
            assert!(s.update(6.5, DT), "6.5 dB is above the close threshold");
        }
    }

    /// One transmission with pauses in it must stay one call.
    #[test]
    fn hang_bridges_the_gaps_inside_speech() {
        let mut s = Squelch::new(8.0, 5.0, 1.5);
        s.update(15.0, DT);
        // A one-second gap, shorter than the hang.
        for _ in 0..20 {
            assert!(s.update(0.0, DT), "should stay open through a 1 s gap");
        }
        s.update(15.0, DT);
        assert_eq!(
            s.trailing_silence_s(),
            0.0,
            "signal returning resets the hang"
        );
    }

    #[test]
    fn it_closes_once_the_hang_expires() {
        let mut s = Squelch::new(8.0, 5.0, 1.0);
        s.update(15.0, DT);
        let mut ticks = 0;
        while s.update(0.0, DT) {
            ticks += 1;
            assert!(ticks < 100, "squelch never closed");
        }
        assert!(
            (ticks as f32 * DT - 1.0).abs() < DT * 1.5,
            "closed after {:.2} s, expected about 1.0",
            ticks as f32 * DT
        );
    }

    /// A close threshold above the open threshold is a configuration mistake
    /// that would make the squelch chatter; it must be neutralised, not obeyed.
    #[test]
    fn an_inverted_configuration_is_clamped() {
        let s = Squelch::new(8.0, 20.0, 1.0);
        assert_eq!(s.close_db, 8.0);
    }

    #[test]
    fn reset_returns_it_to_closed() {
        let mut s = Squelch::default();
        s.update(20.0, DT);
        s.reset();
        assert!(!s.is_open());
    }
}
