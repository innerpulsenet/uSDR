//! Residual carrier tracking that steers a [`DecodeChain`](scannerd_dsp::DecodeChain) NCO.
//!
//! An FM discriminator's DC level *is* the frequency error left after the
//! mixer. Folding that residual back into the mix keeps the carrier in the
//! middle of a tight channel filter instead of riding one skirt — which is
//! what lets the analog and P25 IFs sit at occupied bandwidth rather than
//! the full 12.5 kHz allocation.
//!
//! The loop is a first-order integrator. It only moves while `locked`, so
//! noise with the squelch closed or a Phase 2 framer unlocked cannot walk
//! the NCO off the channel.

/// Mix-frequency tracker: `base + correction`, with the correction integrated
/// from residual discriminator (or sync-fit) error.
pub struct Afc {
    /// Mix frequency without AFC — channel minus LO, or `−TUNE_OFFSET` on
    /// an offset-tuned P25 dongle.
    base_hz: f64,
    /// Accumulated correction. Added to `base_hz` to produce the mix.
    corr_hz: f64,
    /// Fraction of each residual folded in. ~0.05 with 8 ms analog blocks is
    /// a ~160 ms time constant; a C4FM frame at 30/s is similar.
    alpha: f64,
    max_hz: f64,
}

impl Afc {
    /// `max_hz` is the largest |correction| permitted. Analog uses one
    /// deviation (2.5 kHz); digital uses 2 kHz so a wild fit cannot drag
    /// the IF off a 9 kHz C4FM filter.
    pub fn new(base_hz: f64, max_hz: f64) -> Self {
        Self {
            base_hz,
            corr_hz: 0.0,
            alpha: 0.05,
            max_hz: max_hz.max(0.0),
        }
    }

    pub fn with_alpha(mut self, alpha: f64) -> Self {
        self.alpha = alpha.clamp(0.0, 1.0);
        self
    }

    /// Point at a new channel. The integrator is zeroed — a leftover
    /// correction from the previous frequency would be a mistune.
    pub fn set_base(&mut self, base_hz: f64) {
        self.base_hz = base_hz;
        self.corr_hz = 0.0;
    }

    /// Fold in a residual error. `locked` false freezes the integrator.
    pub fn observe(&mut self, residual_hz: f32, locked: bool) {
        if !locked || !residual_hz.is_finite() {
            return;
        }
        self.corr_hz += self.alpha * f64::from(residual_hz);
        self.corr_hz = self.corr_hz.clamp(-self.max_hz, self.max_hz);
    }

    /// Frequency to hand to [`DecodeChain::set_offset`](scannerd_dsp::DecodeChain::set_offset).
    pub fn mix_hz(&self) -> f64 {
        self.base_hz + self.corr_hz
    }

    /// Accumulated correction, for diagnostics. The true carrier error is
    /// this plus whatever residual the discriminator still reports.
    pub fn correction_hz(&self) -> f32 {
        self.corr_hz as f32
    }

    pub fn base_hz(&self) -> f64 {
        self.base_hz
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_steady_residual_is_integrated_into_the_mix() {
        let mut a = Afc::new(-100_000.0, 2_000.0);
        for _ in 0..80 {
            a.observe(400.0, true);
        }
        assert!(
            a.correction_hz() > 200.0,
            "correction only reached {}",
            a.correction_hz()
        );
        assert!((a.mix_hz() + 100_000.0 - f64::from(a.correction_hz())).abs() < 1e-6);
    }

    #[test]
    fn noise_cannot_walk_the_nco_while_unlocked() {
        let mut a = Afc::new(0.0, 2_000.0);
        for _ in 0..200 {
            a.observe(1_500.0, false);
        }
        assert_eq!(a.correction_hz(), 0.0);
    }

    #[test]
    fn the_correction_is_clamped() {
        let mut a = Afc::new(0.0, 500.0).with_alpha(1.0);
        a.observe(5_000.0, true);
        assert!((a.correction_hz() - 500.0).abs() < 1e-3);
        a.observe(-10_000.0, true);
        assert!((a.correction_hz() + 500.0).abs() < 1e-3);
    }

    #[test]
    fn retuning_zeroes_the_integrator() {
        let mut a = Afc::new(1_000.0, 2_000.0).with_alpha(1.0);
        a.observe(300.0, true);
        a.set_base(-50_000.0);
        assert_eq!(a.correction_hz(), 0.0);
        assert_eq!(a.mix_hz(), -50_000.0);
    }
}
