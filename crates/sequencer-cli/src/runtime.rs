//! What a run waits on: the event pumps. The executor that does the running is the
//! crate-private `binds::run` module.

use sequencer_core::input::InputEvent;
use sequencer_core::time::{Clock, Duration, Timestamp};
use sequencer_input::CaptureStream;

/// Why the run loop woke up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    /// The user did something.
    Event(InputEvent),
    /// The deadline arrived, or the wait ended early for no particular reason.
    Deadline,
    /// The event source is gone; shut down.
    Interrupted,
}

/// Something the run loop can wait on.
///
/// Injected rather than hard-wired to a capture backend, so the loop itself can be tested
/// without an operating system underneath it.
pub trait EventPump {
    /// Waits for an event or until `deadline`, whichever comes first.
    ///
    /// `None` means park until an event arrives. Waking early and returning
    /// [`Wake::Deadline`] is allowed — the caller always re-reads the clock.
    fn wait_until(&mut self, deadline: Option<Timestamp>) -> Wake;
}

/// How much of a wait to hand to the spin sleeper rather than the OS.
///
/// Below this the channel's own timeout is too coarse to hit a deadline reliably, so the
/// last slice is spun out instead.
const SPIN_HANDOFF: Duration = Duration::from_millis(2);

/// Waits on a real capture backend.
pub struct CapturePump<'a> {
    stream: CaptureStream,
    clock: &'a dyn Clock,
}

impl<'a> CapturePump<'a> {
    /// Wraps a started capture stream.
    #[must_use]
    pub const fn new(stream: CaptureStream, clock: &'a dyn Clock) -> Self {
        Self { stream, clock }
    }
}

impl std::fmt::Debug for CapturePump<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapturePump")
            .field("dropped", &self.stream.dropped())
            .finish_non_exhaustive()
    }
}

impl EventPump for CapturePump<'_> {
    fn wait_until(&mut self, deadline: Option<Timestamp>) -> Wake {
        let Some(deadline) = deadline else {
            return match self.stream.next_blocking() {
                Some(event) => Wake::Event(event),
                None => Wake::Interrupted,
            };
        };

        let now = self.clock.now();
        if deadline <= now {
            return Wake::Deadline;
        }
        let remaining = deadline.saturating_sub(now);

        if remaining > SPIN_HANDOFF {
            match self
                .stream
                .next_within(remaining.saturating_sub(SPIN_HANDOFF))
            {
                Some(event) => Wake::Event(event),
                None => Wake::Deadline,
            }
        } else {
            self.clock.sleep_until(deadline);
            match self.stream.try_next() {
                Some(event) => Wake::Event(event),
                None => Wake::Deadline,
            }
        }
    }
}

/// Replays a fixed list of events. For testing the loop itself.
#[derive(Debug, Default)]
pub struct ScriptedPump {
    pending: std::collections::VecDeque<(Timestamp, InputEvent)>,
}

impl ScriptedPump {
    /// A pump that will deliver `events` at the given times, in order.
    #[must_use]
    pub fn new(events: impl IntoIterator<Item = (Timestamp, InputEvent)>) -> Self {
        Self {
            pending: events.into_iter().collect(),
        }
    }
}

impl EventPump for ScriptedPump {
    fn wait_until(&mut self, _deadline: Option<Timestamp>) -> Wake {
        match self.pending.pop_front() {
            Some((_, event)) => Wake::Event(event),
            None => Wake::Interrupted,
        }
    }
}
