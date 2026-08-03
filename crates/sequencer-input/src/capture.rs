//! The channel between capture threads and the run loop, and the shared timeline.
//!
//! Capture backends read input on their own threads and the runner consumes it on its
//! own schedule; what connects them is a **bounded, non-blocking** queue. Bounded and
//! non-blocking are the load-bearing properties: if the runner falls behind, events are
//! dropped and counted rather than the reader stalling, and the count is surfaced so a
//! busy machine reads as "events were lost", not "the tool is broken".

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};

use sequencer_core::input::InputEvent;
use sequencer_core::time::Timestamp;

/// The producing half of the queue, held by a capture thread.
#[derive(Debug, Clone)]
pub struct EventQueue {
    tx: SyncSender<InputEvent>,
    dropped: Arc<AtomicU64>,
}

impl EventQueue {
    /// Offers an event, dropping it if the runner is behind.
    ///
    /// Returns whether it was accepted. Never blocks.
    pub fn offer(&self, event: InputEvent) -> bool {
        match self.tx.try_send(event) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }
}

/// The consuming half of the queue, held by the runner.
#[derive(Debug)]
pub struct CaptureStream {
    rx: Receiver<InputEvent>,
    dropped: Arc<AtomicU64>,
}

impl CaptureStream {
    /// Creates both halves of a bounded queue.
    #[must_use]
    pub fn channel(capacity: usize) -> (EventQueue, Self) {
        let (tx, rx) = std::sync::mpsc::sync_channel(capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        (
            EventQueue {
                tx,
                dropped: Arc::clone(&dropped),
            },
            Self { rx, dropped },
        )
    }

    /// Takes the next event if one is waiting.
    pub fn try_next(&self) -> Option<InputEvent> {
        self.rx.try_recv().ok()
    }

    /// Waits up to `timeout` for an event.
    pub fn next_within(&self, timeout: std::time::Duration) -> Option<InputEvent> {
        self.rx.recv_timeout(timeout).ok()
    }

    /// Blocks until an event arrives; `None` means every capture thread has hung up.
    pub fn next_blocking(&self) -> Option<InputEvent> {
        self.rx.recv().ok()
    }

    /// How many events were dropped because the runner could not keep up.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// A monotonically increasing timestamp source shared by capture threads and the runner.
///
/// Capture threads stamp events on the same timeline the engine's deadlines live on,
/// which is what lets the click cadence phase-lock to the physical press. Cloning shares
/// the zero point.
#[derive(Debug, Clone)]
pub struct Epoch(Arc<std::time::Instant>);

impl Epoch {
    /// Starts a new timeline now.
    #[must_use]
    pub fn start() -> Self {
        Self(Arc::new(std::time::Instant::now()))
    }

    /// How far into the timeline it is now.
    #[must_use]
    pub fn now(&self) -> Timestamp {
        Timestamp::from_nanos(u64::try_from(self.0.elapsed().as_nanos()).unwrap_or(u64::MAX))
    }

    /// The instant the timeline started, for building a clock on the same zero.
    #[must_use]
    pub fn instant(&self) -> std::time::Instant {
        *self.0
    }
}

impl Default for Epoch {
    fn default() -> Self {
        Self::start()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sequencer_core::input::{EventKind, Key};

    fn event(n: u64) -> InputEvent {
        InputEvent::physical(Timestamp::from_millis(n), EventKind::KeyDown(Key::F9))
    }

    #[test]
    fn the_queue_round_trips_events_in_order() {
        let (tx, rx) = CaptureStream::channel(8);
        assert!(tx.offer(event(1)));
        assert!(tx.offer(event(2)));
        assert_eq!(rx.try_next(), Some(event(1)));
        assert_eq!(rx.try_next(), Some(event(2)));
        assert_eq!(rx.try_next(), None);
        assert_eq!(rx.dropped(), 0);
    }

    #[test]
    fn a_full_queue_drops_and_counts_rather_than_blocking() {
        let (tx, rx) = CaptureStream::channel(2);
        assert!(tx.offer(event(1)));
        assert!(tx.offer(event(2)));
        // Blocking here would stall a capture thread mid-read, so it must not happen.
        assert!(!tx.offer(event(3)));
        assert_eq!(rx.dropped(), 1);
        assert_eq!(rx.try_next(), Some(event(1)));
    }

    #[test]
    fn offering_after_the_runner_hangs_up_does_not_panic() {
        let (tx, rx) = CaptureStream::channel(2);
        drop(rx);
        assert!(!tx.offer(event(1)));
    }

    #[test]
    fn a_cloned_epoch_shares_its_zero_and_only_moves_forward() {
        let epoch = Epoch::start();
        let twin = epoch.clone();
        let first = epoch.now();
        assert!(twin.now() >= first);
        assert_eq!(epoch.instant(), twin.instant());
    }
}
