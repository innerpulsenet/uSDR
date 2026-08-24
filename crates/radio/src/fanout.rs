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

use num_complex::Complex32;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};

/// One block of baseband samples, shared by every subscriber.
pub type IqBlock = Arc<[Complex32]>;

/// The default queue depth handed to a new subscriber.
///
/// Eight blocks of 16384 samples is ~55 ms at 2.4 MS/s: deep enough to ride out
/// a scheduling hiccup, shallow enough that a wedged consumer is noticed rather
/// than silently accumulating latency.
pub const DEFAULT_DEPTH: usize = 8;

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
        subs.retain(|tx| match tx.try_send(Arc::clone(&block)) {
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

    fn block(n: usize) -> IqBlock {
        vec![Complex32::new(1.0, 0.0); n].into()
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
}
