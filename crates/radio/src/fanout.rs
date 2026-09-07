//! Fan-out from one device's IQ stream to many consumers.
//!
//! hfscan had exactly one consumer of the sample stream, so its radio thread
//! could own a single `SyncSender` and be done. A scanner has several at once —
//! per-channel demodulators, a spectrum producer, a recorder — and they run at
//! different speeds. The rule inherited from hfscan still holds and matters more
//! here: **a slow consumer must never stall the radio thread**, because the
//! device keeps sampling regardless and back-pressure only turns into a USB
//! overrun that costs every other consumer too.
//!
//! So each subscriber gets its own bounded queue and is served with `try_send`.
//! A subscriber that cannot keep up loses blocks, counted per device, and
//! everybody else is unaffected. Blocks are `Arc`'d so fanning out to N
//! consumers costs N refcount bumps rather than N copies of 16k samples.
//!
//! Every block also carries its position on the device's sample timeline
//! (`first_sample`) and the acquisition `epoch` it belongs to. A lost delivery
//! is therefore *detectable* at the next successful block — the consumer sees
//! the sample-index jump — and blocks queued before an accepted tune/rate
//! change carry the old epoch, so a consumer draining a stale queue after a
//! hop can identify and discard them instead of framing across two unrelated
//! sample runs. `ContinuityTracker` implements the consumer-side half.

use num_complex::Complex32;
use std::ops::Deref;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};

/// One block of baseband samples plus its position on the device's sample
/// timeline, shared by every subscriber. The payload is still the same
/// no-copy `Arc<[Complex32]>` fanout as before; `Deref`/`AsRef` keep slice
/// call sites (`block.len()`, `&block[..]`, iteration, `extend_from_slice`)
/// compiling unchanged.
#[derive(Clone, Debug)]
pub struct IqBlock {
    /// Shared sample payload.
    pub samples: Arc<[Complex32]>,
    /// Source sample index of the FIRST sample of this block, at the device's
    /// then-current rate. Monotonically progressing within one acquisition
    /// epoch; blocks may be missing (subscriber overflow = lost delivery),
    /// which shows up as a jump in `first_sample` at the next delivery.
    pub first_sample: u64,
    /// Monotonic acquisition epoch. Bumps on every ACCEPTED tune, accepted
    /// rate change, and device (re)open. Blocks queued before the change keep
    /// the old epoch, so stale old-tune samples are identifiable.
    pub epoch: u64,
}

impl IqBlock {
    /// Sample index one past the last sample (`first_sample + len`).
    pub fn end_sample(&self) -> u64 {
        self.first_sample + self.samples.len() as u64
    }
}

impl Deref for IqBlock {
    type Target = [Complex32];
    fn deref(&self) -> &[Complex32] {
        &self.samples
    }
}

impl AsRef<[Complex32]> for IqBlock {
    fn as_ref(&self) -> &[Complex32] {
        &self.samples
    }
}

/// The default queue depth handed to a new subscriber.
///
/// Eight blocks of 16384 samples is ~55 ms at 2.4 MS/s: deep enough to ride out
/// a scheduling hiccup, shallow enough that a wedged consumer is noticed rather
/// than silently accumulating latency.
pub const DEFAULT_DEPTH: usize = 8;

/// What one received block says about the stream's continuity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GapReport {
    /// True when this block continues the previous run seamlessly.
    pub contiguous: bool,
    /// First block of an epoch (subscriber just joined, or an accepted
    /// tune/rate/reopen): callers must reset decoder/filter state and must
    /// NOT treat this as a gap.
    pub epoch_start: bool,
    /// True when the block's epoch is older than the tracker's current epoch
    /// — a stale queued block from before an acquisition change. Discard it.
    pub stale: bool,
    /// Samples missing between the previous block's end and this block's
    /// start within the same epoch (0 when contiguous or at an epoch start).
    pub lost_samples: u64,
}

/// Consumer-side gap/epoch tracker. Feed it every successfully received
/// block before handing samples to framing paths; it reports whether
/// continuity holds. One tracker per subscriber (each has its own queue and
/// therefore its own loss history).
#[derive(Clone, Debug, Default)]
pub struct ContinuityTracker {
    epoch: Option<u64>,
    next_expected_sample: u64,
}

impl ContinuityTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe a received block and report continuity.
    ///
    /// Out-of-order arrivals are impossible on one `mpsc` queue, so any epoch
    /// different from the last seen one is an epoch boundary. A block whose
    /// epoch is *older* than the tracker's current epoch is stale (queued
    /// before an accepted tune/rate change while the consumer lagged).
    pub fn observe(&mut self, b: &IqBlock) -> GapReport {
        match self.epoch {
            None => {
                // First block this subscriber ever saw.
                self.epoch = Some(b.epoch);
                self.next_expected_sample = b.end_sample();
                GapReport {
                    contiguous: true,
                    epoch_start: true,
                    stale: false,
                    lost_samples: 0,
                }
            }
            Some(cur) if b.epoch < cur => {
                // Stale old-tune block drained from the queue after the
                // epoch already advanced. Do not touch the timeline.
                GapReport {
                    contiguous: false,
                    epoch_start: false,
                    stale: true,
                    lost_samples: 0,
                }
            }
            Some(cur) if b.epoch > cur => {
                // Accepted tune/rate/reopen: a new, unrelated sample run.
                self.epoch = Some(b.epoch);
                self.next_expected_sample = b.end_sample();
                GapReport {
                    contiguous: true,
                    epoch_start: true,
                    stale: false,
                    lost_samples: 0,
                }
            }
            Some(_) => {
                let lost = b.first_sample.saturating_sub(self.next_expected_sample);
                let contiguous = b.first_sample == self.next_expected_sample;
                self.next_expected_sample = b.end_sample();
                GapReport {
                    contiguous,
                    epoch_start: false,
                    stale: false,
                    lost_samples: lost,
                }
            }
        }
    }
}

#[derive(Clone, Default)]
pub struct Fanout {
    subs: Arc<Mutex<Vec<SyncSender<IqBlock>>>>,
}

impl Fanout {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a consumer. Dropping the returned `Receiver` deregisters it on
    /// the next broadcast, so consumers need no explicit teardown.
    pub fn subscribe(&self) -> Receiver<IqBlock> {
        self.subscribe_with_depth(DEFAULT_DEPTH)
    }

    pub fn subscribe_with_depth(&self, depth: usize) -> Receiver<IqBlock> {
        let (tx, rx) = sync_channel(depth.max(1));
        self.subs.lock().expect("fanout mutex").push(tx);
        rx
    }

    pub fn subscriber_count(&self) -> usize {
        self.subs.lock().expect("fanout mutex").len()
    }

    /// Hand `block` to every live subscriber. Returns how many subscribers were
    /// too far behind to take it; disconnected ones are reaped.
    ///
    /// Called from the radio thread once per read, so it must not block. The
    /// mutex is held only for the duration of N `try_send`s — at 2.4 MS/s with
    /// 16k blocks that is ~146 uncontended locks per second.
    pub fn broadcast(&self, block: IqBlock) -> u32 {
        let mut subs = self.subs.lock().expect("fanout mutex");
        let mut lagged = 0;
        subs.retain(|tx| match tx.try_send(block.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                lagged += 1;
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        });
        lagged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(n: usize) -> Arc<[Complex32]> {
        vec![Complex32::new(1.0, 0.0); n].into()
    }

    fn stamped(n: usize, first_sample: u64, epoch: u64) -> IqBlock {
        IqBlock {
            samples: samples(n),
            first_sample,
            epoch,
        }
    }

    /// Plain epoch-0 block for the legacy fanout semantics tests.
    fn block(n: usize) -> IqBlock {
        stamped(n, 0, 0)
    }

    #[test]
    fn every_subscriber_sees_every_block() {
        let f = Fanout::new();
        let a = f.subscribe();
        let b = f.subscribe();
        assert_eq!(f.broadcast(block(4)), 0);
        assert_eq!(a.recv().unwrap().len(), 4);
        assert_eq!(b.recv().unwrap().len(), 4);
    }

    /// The property the whole module exists for: one wedged consumer must not
    /// cost the others a single block.
    #[test]
    fn a_slow_subscriber_loses_blocks_alone() {
        let f = Fanout::new();
        let slow = f.subscribe_with_depth(1);
        let fast = f.subscribe_with_depth(8);
        assert_eq!(f.broadcast(block(1)), 0);
        // `slow` never drains, so its queue is now full.
        for _ in 0..4 {
            assert_eq!(
                f.broadcast(block(1)),
                1,
                "the full queue should be the only one lagging"
            );
        }
        // The fast subscriber still got all five.
        for _ in 0..5 {
            assert!(fast.try_recv().is_ok());
        }
        // And the slow one kept the single block it had room for.
        assert!(slow.try_recv().is_ok());
        assert!(slow.try_recv().is_err());
    }

    #[test]
    fn dropped_receivers_are_reaped() {
        let f = Fanout::new();
        let keep = f.subscribe();
        {
            let _gone = f.subscribe();
            assert_eq!(f.subscriber_count(), 2);
        }
        f.broadcast(block(1));
        assert_eq!(f.subscriber_count(), 1);
        assert!(keep.try_recv().is_ok());
    }

    /// Sample-position metadata: the producer (device thread) stamps the
    /// source index of the first sample; blocks of variable length must show
    /// monotonically progressing positions with no arithmetic on our side.
    #[test]
    fn blocks_carry_progressing_sample_positions() {
        let b = stamped(100, 4096, 0);
        assert_eq!(b.len(), 100);
        assert_eq!(b.end_sample(), 4196);
        // Positions are monotonic when the producer advances by block length,
        // whatever the chunk sizes are.
        let mut pos = 0u64;
        for n in [1usize, 17, 16384, 91, 350, 273] {
            let b = stamped(n, pos, 0);
            assert!(b.end_sample() > pos);
            pos = b.end_sample();
        }
        assert_eq!(pos, 1 + 17 + 16384 + 91 + 350 + 273);
    }

    /// Deref/AsRef compatibility: existing call sites that treat the block as
    /// a slice must keep compiling and working unchanged.
    #[test]
    fn iq_block_still_behaves_like_a_slice() {
        let b = stamped(4, 0, 0);
        assert_eq!(b.len(), 4);
        assert!(!b.is_empty());
        assert_eq!(b[0], Complex32::new(1.0, 0.0));
        assert_eq!(b.iter().count(), 4);
        let s: &[Complex32] = b.as_ref();
        assert_eq!(s.len(), 4);
        let mut copy = Vec::new();
        copy.extend_from_slice(&b);
        assert_eq!(copy.len(), 4);
    }

    /// A slow subscriber that overflows must be able to detect exactly which
    /// samples it lost from the metadata on its next successful delivery —
    /// even with variable block lengths — while a subscriber that kept up
    /// sees perfect continuity.
    #[test]
    fn a_slow_subscriber_can_detect_its_lost_samples() {
        let f = Fanout::new();
        let slow = f.subscribe_with_depth(1);
        let mut pos = 0u64;
        let mut stamp = |n: usize, pos: &mut u64| {
            let b = stamped(n, *pos, 0);
            *pos = b.end_sample();
            b
        };
        assert_eq!(f.broadcast(stamp(1000, &mut pos)), 0);
        // Queue (depth 1) is now full; these overflow for `slow` alone.
        for n in [2000usize, 3000, 700] {
            assert_eq!(f.broadcast(stamp(n, &mut pos)), 1);
        }
        // The slow subscriber must drain its kept block BEFORE the next
        // broadcast can land in its depth-1 queue.
        let kept = slow.try_recv().expect("the block that fit");
        assert_eq!(f.broadcast(stamp(500, &mut pos)), 0);
        let after_gap = slow.try_recv().expect("the next successful delivery");
        // The gap is exactly computable from metadata alone.
        assert_eq!(
            after_gap.first_sample - kept.end_sample(),
            2000 + 3000 + 700,
            "lost-sample span must be visible at the next successful block"
        );
        // And the tracker names it: lost_samples exact, not an epoch start.
        let mut tr = ContinuityTracker::new();
        let r1 = tr.observe(&kept);
        assert!(r1.epoch_start && r1.contiguous);
        let r2 = tr.observe(&after_gap);
        assert!(!r2.contiguous && !r2.epoch_start && !r2.stale);
        assert_eq!(r2.lost_samples, 5700);

        // The healthy story: no gap at all for a subscriber that kept up.
        let f2 = Fanout::new();
        let fast = f2.subscribe();
        let mut pos = 0u64;
        for n in [1000usize, 2000, 3000, 700, 500] {
            assert_eq!(f2.broadcast(stamp(n, &mut pos)), 0);
        }
        let mut tr2 = ContinuityTracker::new();
        for i in 0..5 {
            let b = fast.try_recv().expect("healthy subscriber kept everything");
            let r = tr2.observe(&b);
            assert!(
                r.contiguous && r.lost_samples == 0,
                "block {i} should be contiguous: {r:?}"
            );
            assert_eq!(r.epoch_start, i == 0);
        }
    }

    /// An acquisition change (accepted tune, rate change, reopen) bumps the
    /// epoch. Blocks queued BEFORE the change keep the old epoch, so a
    /// consumer draining its queue after a tune can identify stale blocks and
    /// discard them instead of decoding unrelated sample runs into one frame.
    #[test]
    fn epoch_boundary_identifies_queued_stale_blocks() {
        let f = Fanout::new();
        let sub = f.subscribe_with_depth(4);
        // Two pre-tune blocks fill part of a depth-4 queue; the producer
        // keeps broadcasting after the tune (epoch bump) while this
        // subscriber is momentarily not draining.
        assert_eq!(f.broadcast(stamped(100, 0, 3)), 0);
        assert_eq!(f.broadcast(stamped(100, 100, 3)), 0);
        // Tune accepted → new epoch, sample position restarts.
        assert_eq!(f.broadcast(stamped(100, 0, 4)), 0);
        assert_eq!(f.broadcast(stamped(100, 100, 4)), 0);
        // Consumer drains and sees old-tune blocks queued ahead of new ones.
        let b1 = sub.try_recv().expect("first old block");
        let b2 = sub.try_recv().expect("second old block");
        let b3 = sub.try_recv().expect("first new-epoch block");
        let b4 = sub.try_recv().expect("second new-epoch block");
        assert_eq!((b1.epoch, b2.epoch), (3, 3));
        assert_eq!((b3.epoch, b4.epoch), (4, 4));
        // Within the OLD epoch first_sample continues; the NEW one restarts.
        assert_eq!(b2.first_sample, 100);
        assert_eq!(b3.first_sample, 0);

        // A consumer that was healthy THROUGH the old epoch must see the new
        // blocks as an epoch boundary (reset), not as a gap.
        let mut tr = ContinuityTracker::new();
        assert!(tr.observe(&b1).epoch_start);
        assert!(tr.observe(&b2).contiguous);
        let r3 = tr.observe(&b3);
        assert!(
            r3.epoch_start && !r3.stale && r3.lost_samples == 0,
            "epoch bump must read as a boundary, not a gap: {r3:?}"
        );
        assert!(tr.observe(&b4).contiguous);
    }

    /// A lagging consumer whose queue still holds pre-tune blocks AFTER the
    /// tracker already saw the new epoch must have those stale blocks
    /// identified as stale (discard, not gap, not reset).
    #[test]
    fn stale_blocks_after_epoch_advance_are_flagged() {
        let mut tr = ContinuityTracker::new();
        tr.observe(&stamped(100, 0, 5));
        tr.observe(&stamped(100, 100, 6)); // epoch advanced
        let stale = tr.observe(&stamped(100, 100, 5)); // old queued block
        assert!(stale.stale && !stale.epoch_start && !stale.contiguous);
        assert_eq!(stale.lost_samples, 0);
        // The timeline survives the stale block: the next in-epoch block is
        // still judged against where epoch 6 left off.
        let next = tr.observe(&stamped(100, 200, 6));
        assert!(next.contiguous && !next.stale);
    }

    /// The tracker must not confuse a same-epoch out-of-order arrival with a
    /// gap when blocks merely abut (end == next first), and must count an
    /// exact lost span when they do not.
    #[test]
    fn tracker_reports_exact_lost_spans() {
        let mut tr = ContinuityTracker::new();
        assert!(tr.observe(&stamped(1000, 0, 0)).epoch_start);
        let r = tr.observe(&stamped(500, 2500, 0));
        assert_eq!(r.lost_samples, 1500);
        assert!(!r.contiguous && !r.epoch_start);
        // After the gap the timeline resumes from THIS block's end.
        assert!(tr.observe(&stamped(250, 3000, 0)).contiguous);
    }
}
