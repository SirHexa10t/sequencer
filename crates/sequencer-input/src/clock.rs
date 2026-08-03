//! The real clock.

use std::time::Instant;

use sequencer_core::time::{Clock, Timestamp};

/// A monotonic clock that sleeps accurately enough to hold a click rate.
///
/// `std::thread::sleep` alone is not good enough at the top of the range: granularity is
/// tens of microseconds on Linux and macOS but 1–2 ms on Windows, which is most of a
/// period at 500 clicks per second. `spin_sleep` sleeps natively up to the platform's
/// measured accuracy and then busy-waits the remainder — a few hundred microseconds of
/// spinning per tick, which is a rounding error at 20/s and the difference between
/// hitting the rate and missing it at 500/s.
#[derive(Debug)]
pub struct SystemClock {
    epoch: Instant,
    sleeper: spin_sleep::SpinSleeper,
}

impl SystemClock {
    /// Starts a timeline now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
            sleeper: spin_sleep::SpinSleeper::default(),
        }
    }

    /// Starts a timeline from an existing instant, so a capture thread stamping events
    /// against the same `Instant` shares this clock's zero.
    #[must_use]
    pub fn from_epoch(epoch: Instant) -> Self {
        Self {
            epoch,
            sleeper: spin_sleep::SpinSleeper::default(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_nanos(u64::try_from(self.epoch.elapsed().as_nanos()).unwrap_or(u64::MAX))
    }

    fn sleep_until(&self, deadline: Timestamp) {
        let now = self.now();
        if deadline > now {
            self.sleeper.sleep(deadline.saturating_sub(now));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn time_moves_forward() {
        let clock = SystemClock::new();
        let first = clock.now();
        std::thread::sleep(Duration::from_millis(2));
        assert!(clock.now() > first);
    }

    #[test]
    fn sleeping_until_a_past_deadline_returns_at_once() {
        let clock = SystemClock::new();
        let before = Instant::now();
        clock.sleep_until(Timestamp::ZERO);
        assert!(before.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn sleeping_lands_at_or_after_the_deadline() {
        let clock = SystemClock::new();
        let deadline = clock.now().saturating_add(Duration::from_millis(15));
        clock.sleep_until(deadline);
        assert!(clock.now() >= deadline);
    }
}
