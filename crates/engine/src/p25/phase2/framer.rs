//! Burst framing and superframe tracking for Phase 2.
//!
//! The front end yields one frequency estimate per sample; this finds the
//! symbol phase and burst boundaries in that stream and hands up whole 180-dibit
//! bursts, each tagged with where it sits in the 12-burst superframe.
//!
//! Acquisition leans on the ISCH: the fixed **S-ISCH** sync word leads half the
//! bursts (superframe positions 2, 3, 6, 7, 10, 11), so scanning every sample
//! phase for it locks both the symbol timing and the burst boundary at once —
//! the same "let the correlation pick the alignment" approach the Phase 1
//! detector uses, adapted to a word that recurs every other burst instead of a
//! per-frame sync. Once locked, each following burst is read at a fixed stride
//! and its ISCH re-decoded, which keeps the superframe slot counter honest and
//! re-checks the lock every burst.

use super::isch::{self, Isch};
use super::{
    BURST_DIBITS, DEV_OUTER_HZ, ISCH_DIBITS, PAYLOAD_DIBITS, SUPERFRAME_BURSTS, SYMBOL_RATE,
    dibit_level, fit_symbols, slice_at,
};

/// Sliced ISCH dibits allowed to disagree with S-ISCH during acquisition. The
/// code corrects 7, but acquisition uses a tighter bound so noise cannot
/// masquerade as a sync while scanning every sample phase of a long buffer.
const ACQUIRE_MAX_ERRORS: u32 = 4;

/// Consecutive bursts whose ISCH fails to decode before the lock is dropped.
const LOSS_LIMIT: u32 = 6;

/// One decoded burst: its superframe position and its raw payload dibits.
#[derive(Clone, Debug)]
pub struct Burst {
    /// Position within the superframe, 0..=11, from the slot tracker.
    pub superframe_slot: u8,
    /// The ISCH that led the burst, already classified.
    pub isch: Isch,
    /// The 160 payload dibits, still scrambled.
    pub payload: [u8; PAYLOAD_DIBITS],
}

/// The op25 superframe-position table: which ISCH `checkval` each of the 12
/// positions must carry. `-2` marks an S-ISCH (synchronisation) position.
const EXPECTED: [i32; SUPERFRAME_BURSTS] = [0, 1, -2, -2, 4, 5, -2, -2, 8, 9, -2, -2];

/// Tracks position within the superframe, a port of op25's `p25p2_sync`.
///
/// Each burst advances the counter; an I-ISCH whose `checkval` matches the
/// expected value for the counter confirms the position, and a mismatch drops
/// confidence so the framer re-acquires rather than trusting a slipped count.
struct SlotTracker {
    slotid: usize,
    confident: bool,
}

impl SlotTracker {
    fn new() -> Self {
        Self {
            slotid: 0,
            confident: false,
        }
    }

    /// Fold in one burst's ISCH, returning the resulting superframe position.
    fn observe(&mut self, isch: Isch) -> u8 {
        self.slotid += 1;
        if self.slotid >= SUPERFRAME_BURSTS {
            self.slotid = 0;
        }
        let checkval = match isch {
            Isch::Sync => -2,
            Isch::Info(info) => info.checkval(),
            Isch::Unknown => -1,
        };
        // A definite value that disagrees with the expected pattern means we
        // have lost the count.
        if checkval != -1 && EXPECTED[self.slotid] != checkval {
            self.confident = false;
        }
        // An I-ISCH names its own position outright; trust it and resync.
        if let Isch::Info(info) = isch {
            self.confident = true;
            self.slotid = (info.checkval() as usize).min(SUPERFRAME_BURSTS - 1);
        }
        self.slotid as u8
    }
}

/// Finds and tracks Phase 2 bursts in a frequency-sample stream.
pub struct Phase2Framer {
    buf: Vec<f32>,
    consumed: u64,
    /// Absolute sample index of the next burst's first symbol, once locked.
    /// Fractional: a conventional scanner's channel rate is not an integer
    /// number of samples per Phase 2 symbol (2.048 MS/s ÷ 43 → 7.938 sps),
    /// so a whole-sample stride slips a full symbol within a couple of
    /// bursts and the voice follower drops lock.
    next_start: Option<f64>,
    /// `fs / 6000`, not necessarily an integer.
    sps: f64,
    tracker: SlotTracker,
    losses: u32,
    /// Bursts seen since construction, for diagnostics.
    pub bursts: u64,
    /// Bursts whose ISCH was S-ISCH or a valid I-ISCH.
    pub isch_ok: u64,
    /// Smallest Hamming distance to S-ISCH seen while scanning for lock — a
    /// diagnostic of how close acquisition is getting. 0 means an exact sync
    /// was found; a value stuck near 20 means the demod is not producing the
    /// right dibits at all.
    pub acq_best: u32,
    /// Fitted outer deviation, Hz. Starts at the standard 2250 and follows
    /// each S-ISCH so a low-deviation or AGC-scaled burst still slices.
    amp: f32,
    /// Residual carrier still sitting on the frequency samples, Hz.
    offset: f32,
}

impl Default for Phase2Framer {
    fn default() -> Self {
        Self::new(super::CHANNEL_RATE)
    }
}

impl Phase2Framer {
    pub fn new(fs: f64) -> Self {
        Self {
            buf: Vec::new(),
            consumed: 0,
            next_start: None,
            sps: fs / SYMBOL_RATE,
            tracker: SlotTracker::new(),
            losses: 0,
            bursts: 0,
            isch_ok: 0,
            acq_best: 40,
            amp: DEV_OUTER_HZ,
            offset: 0.0,
        }
    }

    fn burst_samples(&self) -> f64 {
        BURST_DIBITS as f64 * self.sps
    }

    /// Linear interpolation so symbol centres can sit between ADC samples.
    fn hz_at(&self, abs: f64) -> f32 {
        let rel = abs - self.consumed as f64;
        if rel < 0.0 {
            return 0.0;
        }
        let i = rel.floor() as usize;
        let frac = (rel - i as f64) as f32;
        let a = match self.buf.get(i) {
            Some(&v) => v,
            None => return 0.0,
        };
        let b = self.buf.get(i + 1).copied().unwrap_or(a);
        a + frac * (b - a)
    }

    fn slice_abs(&self, abs: f64) -> u8 {
        slice_at(self.hz_at(abs) - self.offset, self.amp)
    }

    /// Whether the framer currently holds superframe lock.
    pub fn locked(&self) -> bool {
        self.next_start.is_some() && self.tracker.confident
    }

    /// Feed frequency samples; returns whole bursts that became available.
    pub fn push(&mut self, samples: &[f32]) -> Vec<Burst> {
        self.buf.extend_from_slice(samples);
        let mut out = Vec::new();
        let burst_samples = self.burst_samples();

        loop {
            match self.next_start {
                // Locked: read the next burst if it has fully arrived.
                Some(start) => {
                    let rel = start - self.consumed as f64;
                    // Last symbol sits at (BURST_DIBITS-1)*sps; +1 for the interpolator.
                    let need = rel + (BURST_DIBITS as f64 - 1.0) * self.sps + 1.0;
                    if need > self.buf.len() as f64 {
                        break;
                    }
                    let (isch, payload) = self.read_burst(start);
                    self.bursts += 1;
                    let ok = !matches!(isch, Isch::Unknown);
                    if ok {
                        self.isch_ok += 1;
                        self.losses = 0;
                    } else {
                        self.losses += 1;
                    }
                    let slot = self.tracker.observe(isch);
                    out.push(Burst {
                        superframe_slot: slot,
                        isch,
                        payload,
                    });
                    if self.losses >= LOSS_LIMIT {
                        // Too many bad bursts: drop lock and re-acquire.
                        self.next_start = None;
                        self.tracker = SlotTracker::new();
                        self.losses = 0;
                    } else {
                        let off = self.isch_sample_offset(start, isch);
                        self.next_start = Some(start + burst_samples + off);
                    }
                }
                // Unlocked: scan every sample phase for an S-ISCH.
                None => match self.acquire() {
                    Some(abs_start) => {
                        self.next_start = Some(abs_start);
                    }
                    None => break,
                },
            }
        }

        // Retain only what an in-flight burst plus a re-acquisition scan needs.
        let keep = (burst_samples * 2.0 + self.sps).ceil() as usize + 2;
        if self.buf.len() > keep {
            let drop = self.buf.len() - keep;
            // Never drop past a pending burst start.
            let limit = match self.next_start {
                Some(s) => (s - self.consumed as f64).max(0.0) as usize,
                None => drop,
            };
            let drop = drop.min(limit);
            if drop > 0 {
                self.buf.drain(..drop);
                self.consumed += drop as u64;
            }
        }
        out
    }

    /// Read one burst starting at absolute sample `start`: its ISCH and payload.
    fn read_burst(&mut self, start: f64) -> (Isch, [u8; PAYLOAD_DIBITS]) {
        let mut isch_dibits = [0u8; ISCH_DIBITS];
        for (j, d) in isch_dibits.iter_mut().enumerate() {
            *d = self.slice_abs(start + j as f64 * self.sps);
        }
        let isch = isch::decode_dibits(&isch_dibits);
        let known_cw = match isch {
            Isch::Sync => Some(isch::S_ISCH),
            Isch::Info(info) => isch::codeword_for(info.value),
            Isch::Unknown => None,
        };
        if let Some(cw) = known_cw {
            let mut samples = [0.0f32; ISCH_DIBITS];
            let mut ideal = [0.0f32; ISCH_DIBITS];
            for j in 0..ISCH_DIBITS {
                samples[j] = self.hz_at(start + j as f64 * self.sps);
                let dibit = ((cw >> (38 - 2 * j)) & 0b11) as u8;
                ideal[j] = dibit_level(dibit);
            }
            let (amp, offset) = fit_symbols(&samples, &ideal);
            // Slow follow so one corrupted sync cannot slam the slicer.
            self.amp = 0.7 * self.amp + 0.3 * amp;
            self.offset = 0.7 * self.offset + 0.3 * offset;
        }
        let mut payload = [0u8; PAYLOAD_DIBITS];
        for (j, d) in payload.iter_mut().enumerate() {
            let sym = ISCH_DIBITS + j;
            *d = self.slice_abs(start + sym as f64 * self.sps);
        }
        (isch, payload)
    }

    /// Best sample-phase offset of a known ISCH, in samples (can be negative).
    fn isch_sample_offset(&self, start: f64, isch: Isch) -> f64 {
        let cw = match isch {
            Isch::Sync => isch::S_ISCH,
            Isch::Info(info) => match isch::codeword_for(info.value) {
                Some(w) => w,
                None => return 0.0,
            },
            Isch::Unknown => return 0.0,
        };
        let mut best_dist = u32::MAX;
        let mut best_off = 0.0f64;
        let limit = (self.sps * 0.6).max(1.0);
        let mut off = -limit;
        while off <= limit {
            let mut word = 0u64;
            for j in 0..ISCH_DIBITS {
                word = (word << 2) | u64::from(self.slice_abs(start + off + j as f64 * self.sps));
            }
            let dist = (word ^ cw).count_ones();
            if dist < best_dist {
                best_dist = dist;
                best_off = off;
            }
            off += 0.25;
        }
        best_off
    }

    /// Scan for an S-ISCH at any sample phase; returns its absolute burst start.
    ///
    /// Do not lock to the first word that merely falls inside the acquisition
    /// threshold.  The samples on either shoulder of a symbol can slice into a
    /// nearly-correct word before the eye-centre sample arrives.  Comparing the
    /// complete buffered scan and taking its smallest Hamming distance gives us
    /// the most reliable symbol phase available.
    fn acquire(&mut self) -> Option<f64> {
        // A burst needs ISCH + payload present to be worth locking onto.
        // The last symbol is at (BURST_DIBITS-1)*sps; do not require a
        // whole extra burst-length of slack or we miss the true start.
        let last_sym = (BURST_DIBITS as f64 - 1.0) * self.sps;
        if (self.buf.len() as f64) < last_sym + 1.0 {
            return None;
        }
        let last_start = (self.buf.len() as f64 - last_sym - 1.0).floor().max(0.0) as usize;
        let mut best: Option<(u32, usize)> = None;
        for start in 0..=last_start {
            let mut cw = 0u64;
            for j in 0..ISCH_DIBITS {
                let abs = self.consumed as f64 + start as f64 + j as f64 * self.sps;
                cw = (cw << 2) | u64::from(self.slice_abs(abs));
            }
            let dist = (cw ^ isch::S_ISCH).count_ones();
            self.acq_best = self.acq_best.min(dist);
            if dist <= ACQUIRE_MAX_ERRORS && best.is_none_or(|(best_dist, _)| dist < best_dist) {
                best = Some((dist, start));
            }
        }
        let (_, start) = best?;
        let abs = self.consumed as f64 + start as f64;
        // Drop everything before this so the buffer stays bounded.
        if start > 0 {
            self.buf.drain(..start);
            self.consumed += start as u64;
        }
        // An S-ISCH position is one of {2,3,6,7,10,11}; seed the tracker
        // just before position 2 so the first observe lands there.
        self.tracker = SlotTracker::new();
        self.tracker.slotid = 1;
        self.tracker.confident = true;
        Some(abs)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::modulate;
    use super::super::{CHANNEL_RATE, ISCH_DIBITS, SPS};
    use super::*;

    /// Build a burst's 180 dibits: a leading ISCH codeword then arbitrary
    /// payload. `isch_cw` is the 40-bit ISCH to embed.
    fn burst_dibits(isch_cw: u64, payload_seed: u8) -> Vec<u8> {
        let mut d = Vec::with_capacity(BURST_DIBITS);
        for j in 0..ISCH_DIBITS {
            d.push(((isch_cw >> (38 - 2 * j)) & 3) as u8);
        }
        let mut s = payload_seed;
        for _ in 0..PAYLOAD_DIBITS {
            s = s.wrapping_mul(37).wrapping_add(11);
            d.push((s >> 3) & 3);
        }
        d
    }

    /// The smallest info value whose `checkval` equals `want`.
    fn value_with_checkval(want: i32) -> u16 {
        (0u16..128)
            .find(|&v| isch::IschInfo::from_value(v).checkval() == want)
            .expect("some I-ISCH has this checkval")
    }

    /// A superframe's worth of bursts, ISCH codewords matching the expected
    /// per-position pattern.
    fn superframe() -> Vec<u8> {
        let mut d = Vec::new();
        for (pos, &want) in EXPECTED.iter().enumerate() {
            let cw = if want < 0 {
                isch::S_ISCH
            } else {
                isch::codeword_for(value_with_checkval(want)).unwrap()
            };
            d.extend(burst_dibits(cw, pos as u8 + 1));
        }
        d
    }

    #[test]
    fn it_locks_onto_a_clean_superframe() {
        // Two superframes so the framer has a full one after acquisition.
        let mut dibits = superframe();
        dibits.extend(superframe());
        let iq = modulate(&dibits, CHANNEL_RATE);
        let mut front = super::super::Phase2FrontEnd::new(CHANNEL_RATE);
        let mut hz = Vec::new();
        front.process(&iq, &mut hz);

        let mut framer = Phase2Framer::new(CHANNEL_RATE);
        let bursts = framer.push(&hz);
        assert!(
            bursts.len() >= SUPERFRAME_BURSTS,
            "expected at least a superframe of bursts, got {}",
            bursts.len()
        );
        // Most ISCHs should decode; the very first burst after acquisition may
        // be mid-stream.
        let ok = bursts
            .iter()
            .filter(|b| !matches!(b.isch, Isch::Unknown))
            .count();
        assert!(ok >= SUPERFRAME_BURSTS - 2, "only {ok} ISCHs decoded");
    }

    #[test]
    fn acquisition_prefers_the_cleanest_sync_over_the_first_acceptable_one() {
        let first = burst_dibits(isch::S_ISCH ^ 0x0f, 17);
        let second = burst_dibits(isch::S_ISCH, 91);
        let expected = second[ISCH_DIBITS..].to_vec();
        let hz: Vec<f32> = first
            .into_iter()
            .chain(second)
            .flat_map(|dibit| std::iter::repeat_n(super::super::dibit_level(dibit), SPS))
            .collect();

        let mut framer = Phase2Framer::new(CHANNEL_RATE);
        let bursts = framer.push(&hz);

        assert_eq!(framer.acq_best, 0);
        assert_eq!(bursts.len(), 1);
        assert_eq!(bursts[0].payload.as_slice(), expected.as_slice());
    }

    #[test]
    fn it_locks_onto_a_superframe_at_the_analog_channel_rate() {
        use super::super::tests::modulate_timed;

        let analog_hz = 2_048_000.0 / 43.0;
        let mut dibits = superframe();
        dibits.extend(superframe());
        dibits.extend(superframe());
        let iq = modulate_timed(&dibits, analog_hz);
        let mut front = super::super::Phase2FrontEnd::new(analog_hz);
        let mut hz = Vec::new();
        front.process(&iq, &mut hz);

        let mut framer = Phase2Framer::new(analog_hz);
        let bursts = framer.push(&hz);
        let ok = bursts
            .iter()
            .filter(|b| !matches!(b.isch, Isch::Unknown))
            .count();
        assert!(
            bursts.len() >= SUPERFRAME_BURSTS,
            "expected a superframe at analog rate, got {}",
            bursts.len()
        );
        assert!(
            ok >= SUPERFRAME_BURSTS - 2,
            "integer stride used to lose lock at 7.938 sps; {ok} good ISCHs / {}",
            bursts.len()
        );
        assert!(framer.locked());
    }

    #[test]
    fn analog_rate_receiver_holds_lock_through_decode_chain() {
        use super::super::tests::modulate_timed;
        use scannerd_dsp::DecodeChain;

        let mut dibits = superframe();
        dibits.extend(superframe());
        dibits.extend(superframe());
        let fs = 2_048_000.0;
        let iq = modulate_timed(&dibits, fs);
        let mut chain = DecodeChain::new(fs, super::super::PHASE2_BANDWIDTH_HZ, CHANNEL_RATE);
        let mut bb = Vec::new();
        chain.process(&iq, &mut bb);
        let mut rx = super::super::Phase2Receiver::new(chain.fs_out());
        let mut bursts = Vec::new();
        for chunk in bb.chunks(2_048) {
            bursts.extend(rx.process(chunk));
        }
        let ok = bursts
            .iter()
            .filter(|b| !matches!(b.isch, Isch::Unknown))
            .count();
        assert!(
            bursts.len() >= SUPERFRAME_BURSTS,
            "expected a superframe through DecodeChain, got {}",
            bursts.len()
        );
        assert!(
            ok >= SUPERFRAME_BURSTS - 2,
            "integer stride used to drop lock at analog rate; {ok} good ISCHs / {}",
            bursts.len()
        );
        assert!(
            rx.locked(),
            "framer should still hold lock at the analog channel rate"
        );
    }

    #[test]
    fn the_slot_tracker_follows_the_expected_pattern() {
        let mut t = SlotTracker::new();
        t.slotid = 1;
        // Feed S-ISCH then I-ISCH values in order and confirm the counter walks.
        let mut positions = Vec::new();
        for &want in EXPECTED.iter().skip(2) {
            let isch = if want < 0 {
                Isch::Sync
            } else {
                Isch::Info(isch::IschInfo::from_value(value_with_checkval(want)))
            };
            positions.push(t.observe(isch));
        }
        assert_eq!(positions[0], 2);
    }
}
