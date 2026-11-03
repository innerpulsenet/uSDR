//! AGC speed, shared by the AM and sideband demodulators.
//!
//! Medium is the speed both had before it was a choice: AM's 50 ms mean-level
//! loop, and sideband's 400 ms hang recovering at 15 dB/s. Fast follows a
//! fluttering signal and recovers quickly after a strong station unkeys; slow
//! rides through long pauses and deep fades without the noise rising
//! between words.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgcSpeed {
    Fast,
    #[default]
    Medium,
    Slow,
}

impl AgcSpeed {
    /// The AM mean-level loop's time constant, seconds.
    pub fn am_tau_s(self) -> f32 {
        match self {
            AgcSpeed::Fast => 0.02,
            AgcSpeed::Medium => 0.05,
            AgcSpeed::Slow => 0.25,
        }
    }

    /// How long sideband's gain holds after the last peak, seconds.
    pub fn ssb_hang_s(self) -> f32 {
        match self {
            AgcSpeed::Fast => 0.10,
            AgcSpeed::Medium => 0.40,
            AgcSpeed::Slow => 1.20,
        }
    }

    /// How fast sideband's gain recovers after the hang, dB/s.
    pub fn ssb_decay_db_per_s(self) -> f32 {
        match self {
            AgcSpeed::Fast => 40.0,
            AgcSpeed::Medium => 15.0,
            AgcSpeed::Slow => 6.0,
        }
    }
}
