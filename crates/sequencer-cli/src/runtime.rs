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
    heartbeat: Option<Heartbeat<'a>>,
}

/// A beat sent while the pump waits: what a supervised grab owes its supervisor.
struct Heartbeat<'a> {
    every: Duration,
    next: Timestamp,
    /// Sends one beat; `false` once nobody is listening any more.
    send: Box<dyn FnMut() -> bool + 'a>,
}

impl<'a> CapturePump<'a> {
    /// Wraps a started capture stream.
    #[must_use]
    pub const fn new(stream: CaptureStream, clock: &'a dyn Clock) -> Self {
        Self {
            stream,
            clock,
            heartbeat: None,
        }
    }

    /// Sends a beat with `send` at least every `every` while the run waits on this pump —
    /// a running sequence waits here too, so a long one never reads as a hang. Once `send`
    /// reports nobody listening, every wait answers [`Wake::Interrupted`], which ends the
    /// run.
    #[must_use]
    pub fn beating(mut self, every: Duration, send: impl FnMut() -> bool + 'a) -> Self {
        self.heartbeat = Some(Heartbeat {
            every,
            next: self.clock.now(),
            send: Box::new(send),
        });
        self
    }

    /// Sends the beat if one is due: `false` when it went unheard.
    fn beat(&mut self) -> bool {
        let Some(heartbeat) = &mut self.heartbeat else {
            return true;
        };
        let now = self.clock.now();
        if now < heartbeat.next {
            return true;
        }
        heartbeat.next = now.saturating_add(heartbeat.every);
        (heartbeat.send)()
    }

    /// `deadline`, brought forward to the next beat if that comes first.
    fn capped(&self, deadline: Option<Timestamp>) -> Option<Timestamp> {
        match (&self.heartbeat, deadline) {
            (None, deadline) => deadline,
            (Some(heartbeat), None) => Some(heartbeat.next),
            (Some(heartbeat), Some(deadline)) => Some(deadline.min(heartbeat.next)),
        }
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
        // A beat due goes first, and the next one caps the wait: waking early with
        // `Deadline` is this trait's contract, and the caller re-reads the clock and waits.
        if !self.beat() {
            return Wake::Interrupted;
        }
        let Some(deadline) = self.capped(deadline) else {
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    /// A beating pump beats as soon as it waits and once a period after that, waking early
    /// for each — so a long wait never goes quiet — and once nobody hears a beat, the wait
    /// ends the run.
    #[test]
    fn a_beating_pump_beats_through_long_waits_and_stops_unheard() {
        let clock = sequencer_input::SystemClock::new();
        // Held, not dropped: a closed queue would end every wait at once.
        let (_queue, stream) = CaptureStream::channel(4);
        let beats = Cell::new(0_u32);
        let heard = Cell::new(true);
        let mut pump = CapturePump::new(stream, &clock).beating(Duration::from_millis(20), || {
            beats.set(beats.get() + 1);
            heard.get()
        });
        let deadline = clock.now().saturating_add(Duration::from_millis(200));
        let mut wakes = 0;
        while clock.now() < deadline {
            assert_eq!(pump.wait_until(Some(deadline)), Wake::Deadline);
            wakes += 1;
        }
        assert!(
            beats.get() >= 8,
            "a beat a period, over ten periods: {}",
            beats.get()
        );
        assert!(wakes >= 8, "woken for the beats: {wakes}");

        heard.set(false);
        let give_up = clock.now().saturating_add(Duration::from_secs(1));
        loop {
            match pump.wait_until(Some(give_up)) {
                Wake::Interrupted => break,
                Wake::Deadline => assert!(
                    clock.now() < give_up,
                    "the unheard beat never ended the run"
                ),
                Wake::Event(event) => panic!("no event was sent: {event:?}"),
            }
        }
    }
}
