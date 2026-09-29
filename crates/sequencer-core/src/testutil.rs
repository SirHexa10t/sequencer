//! The virtual clock every crate above tests against.
//!
//! Exported unconditionally rather than behind a feature, because the same double powers
//! the CLI's tests. A tool in this space lives or dies on whether a user's "it does the
//! wrong thing" can be turned into a reproducible case without their hardware — and a
//! clock that only moves when told to is what makes "the release was noticed mid-wait,
//! not after the 10s WAIT" an exact assertion that completes in microseconds.

use alloc::vec::Vec;
use core::cell::{Cell, RefCell};

use crate::time::{Clock, Duration, Timestamp};

/// A clock that only moves when told to.
///
/// Time travel is the point: a thousand simulated seconds cost microseconds, so the
/// timing assertions that actually matter for an autoclicker are cheap enough to run on
/// every commit.
#[derive(Debug, Default)]
pub struct VirtualClock {
    now: Cell<Timestamp>,
    /// How far past a requested deadline [`Clock::sleep_until`] lands.
    ///
    /// Simulates a busy machine descheduling the process. Set it to 500 ms to exercise
    /// the catch-up clamp without needing a loaded machine to reproduce it on.
    overshoot: Cell<Duration>,
    slept: RefCell<Vec<(Timestamp, Timestamp)>>,
}

impl VirtualClock {
    /// A clock at the epoch.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A clock at `start`.
    #[must_use]
    pub fn starting_at(start: Timestamp) -> Self {
        let clock = Self::default();
        clock.now.set(start);
        clock
    }

    /// Moves time forward by `delta`.
    pub fn advance(&self, delta: Duration) {
        self.now.set(self.now.get().saturating_add(delta));
    }

    /// Moves time to `to`, in either direction.
    ///
    /// Going backwards is allowed on purpose: NTP steps and suspend/resume do it to real
    /// clocks, and the engine has to survive it.
    pub fn set(&self, to: Timestamp) {
        self.now.set(to);
    }

    /// How far past a deadline this clock lands.
    #[must_use]
    pub fn overshoot(&self) -> Duration {
        self.overshoot.get()
    }

    /// Sets how far past a deadline this clock lands.
    pub fn set_overshoot(&self, by: Duration) {
        self.overshoot.set(by);
    }

    /// Every `(requested_deadline, arrived_at)` pair so far.
    #[must_use]
    pub fn sleeps(&self) -> Vec<(Timestamp, Timestamp)> {
        self.slept.borrow().clone()
    }
}

impl Clock for VirtualClock {
    fn now(&self) -> Timestamp {
        self.now.get()
    }

    fn sleep_until(&self, deadline: Timestamp) {
        let landed = deadline.saturating_add(self.overshoot.get());
        // Never move backwards, so a deadline already in the past is a no-op rather than
        // a rewind.
        let landed = landed.max(self.now.get());
        self.slept.borrow_mut().push((deadline, landed));
        self.now.set(landed);
    }
}
