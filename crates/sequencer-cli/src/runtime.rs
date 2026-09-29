//! What a run waits on and what it reports: the event pumps, the exit summary, and the
//! sink wrapper that fills it. The executor that does the running is the crate-private
//! `binds::run` module.

use sequencer_core::emit::InputSink;
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

    /// How many events were lost because the loop could not keep up.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.stream.dropped()
    }
}

impl std::fmt::Debug for CapturePump<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapturePump")
            .field("dropped", &self.dropped())
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

/// The period a rate implies, for the burst-accounting window below.
///
/// Falls back to a tenth of a second for a rate with no sensible period (zero or
/// non-finite). Such a rate never reaches a run — the profile rejects it first — and the
/// window only ever tells one burst from another, so a placeholder is harmless where a
/// panic would not be.
#[must_use]
pub fn cadence_of(cps: f64) -> Duration {
    sequencer_core::time::Period::from_cps(cps).map_or(Duration::from_millis(100), |period| {
        Duration::from_nanos(period.nanos())
    })
}

/// The longest gap between two repetitions that still counts as one continuous burst.
///
/// Four times the requested period, and at least a quarter-second. Generous on purpose: a
/// machine that stutters must not be mistaken for the user letting go. Still far shorter than
/// any real pause between two deliberate presses, which is the thing being excluded.
fn burst_gap(cadence: Duration) -> Duration {
    (cadence * 4).max(Duration::from_millis(250))
}

/// A sink that counts what passes through it, for the exit report of a run whose
/// executor keeps no statistics of its own.
///
/// One action is the *repetition marker* — the press that begins each iteration — and
/// its timestamps drive the burst accounting behind [`RunSummary::sent_cps`]: idle time
/// between activations is excluded, a stutter within one is not.
pub(crate) struct CountingSink<'a> {
    inner: &'a mut dyn InputSink,
    marker: sequencer_core::emit::EmitAction,
    same_burst: Duration,
    previous: Option<Timestamp>,
    summary: RunSummary,
}

impl<'a> CountingSink<'a> {
    /// Wraps `inner`, counting an iteration every time `marker` goes through, with
    /// `cadence` sizing the burst window as [`burst_gap`] does.
    pub(crate) fn new(
        inner: &'a mut dyn InputSink,
        marker: sequencer_core::emit::EmitAction,
        cadence: Duration,
    ) -> Self {
        Self {
            inner,
            marker,
            same_burst: burst_gap(cadence),
            previous: None,
            summary: RunSummary::default(),
        }
    }

    /// The report, once the run is over.
    pub(crate) fn finish(self, wall: Duration) -> RunSummary {
        RunSummary {
            wall,
            ..self.summary
        }
    }
}

impl InputSink for CountingSink<'_> {
    fn emit(
        &mut self,
        emit: &sequencer_core::emit::Emit,
    ) -> Result<(), sequencer_core::emit::SinkError> {
        self.inner.emit(emit)?;
        self.summary.emitted += 1;
        if emit.action == self.marker {
            self.summary.iterations += 1;
            if let Some(previous) = self.previous {
                let gap = emit.at.saturating_sub(previous);
                if gap <= self.same_burst {
                    self.summary.active += gap;
                    self.summary.paced_spans += 1;
                }
            }
            self.previous = Some(emit.at);
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), sequencer_core::emit::SinkError> {
        self.inner.flush()
    }

    fn release_all(&mut self) {
        self.inner.release_all();
    }
}

/// What a run did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunSummary {
    /// Actions delivered to the sink.
    pub emitted: u64,
    /// Iterations begun.
    pub iterations: u64,
    /// How long the repeater was actually firing, summed across activations.
    ///
    /// Genuinely summed: the gap between two repetitions counts only when they belong to
    /// the same burst. Measuring first-to-last instead would fold every pause BETWEEN
    /// activations into the total — tap the trigger a few times over a minute and a perfect
    /// 20/s reads as 3/s, which looks like a broken tool rather than a user thinking.
    pub active: Duration,
    /// Wall-clock length of the whole run, first tick to shutdown.
    ///
    /// Unlike [`Self::active`] this includes idle time waiting for the trigger — it is a
    /// stopwatch for the user, not a denominator for a rate. Dividing anything by it would
    /// recreate the bug `active` exists to avoid.
    pub wall: Duration,
    /// Gaps counted into [`Self::active`] — the denominator's matching numerator.
    ///
    /// Not `iterations - 1`: that counts the spans BETWEEN bursts too, which `active`
    /// deliberately excludes, and dividing one by the other would understate the rate by
    /// exactly the share of repetitions that started a burst.
    pub paced_spans: u64,
}

impl RunSummary {
    /// Repetitions per second this process actually *sent*, if enough happened to say.
    ///
    /// The number that matters when the requested rate is near what the machine can do:
    /// "you asked for 5000 and sent 3200" is the answer, and a rate the tool silently
    /// failed to hit would otherwise look like success.
    ///
    /// Deliberately not called "achieved": it counts events handed to the backend, and the
    /// input stack above can still coalesce or drop them. `bench` is the one that measures
    /// what came back out of the kernel.
    #[must_use]
    pub fn sent_cps(&self) -> Option<f64> {
        let seconds = self.active.as_secs_f64();
        (self.paced_spans > 0 && seconds > 0.0).then(|| {
            #[allow(
                clippy::cast_precision_loss,
                reason = "a count this large would need centuries of clicking"
            )]
            let spans = self.paced_spans as f64;
            spans / seconds
        })
    }
}

#[cfg(test)]
mod summary_tests {
    use super::*;

    /// The bug this replaced: two short bursts a long way apart used to be measured
    /// first-repetition-to-last, folding the idle minute between them into "active" and
    /// reporting a fraction of the real rate.
    #[test]
    fn idle_time_between_activations_is_not_counted_as_clicking() {
        let mut summary = RunSummary::default();
        // Two bursts of 20/s (50ms apart), separated by a 30-second pause.
        for _ in 0..10 {
            summary.active += Duration::from_millis(50);
            summary.paced_spans += 1;
        }
        summary.iterations = 22; // 11 repetitions per burst; the pause spans none
        assert_eq!(
            summary.sent_cps().map(f64::round),
            Some(20.0),
            "the pause between bursts must not drag the rate down"
        );
    }

    /// `iterations - 1` would count the between-burst span as a gap that `active` never
    /// included — the exact mismatch that made the old figure look like a slow machine.
    #[test]
    fn the_numerator_counts_only_the_gaps_the_denominator_measured() {
        let summary = RunSummary {
            iterations: 100,
            paced_spans: 10,
            active: Duration::from_secs(1),
            ..RunSummary::default()
        };
        assert_eq!(summary.sent_cps(), Some(10.0), "spans, not iterations");
    }

    /// A single repetition spans no gap, so there is nothing to divide — better no figure
    /// than a fabricated one.
    #[test]
    fn one_lonely_repetition_reports_no_rate() {
        let summary = RunSummary {
            iterations: 1,
            ..RunSummary::default()
        };
        assert_eq!(summary.sent_cps(), None);
    }

    /// The counting sink measures like the engine's summary does: the marker counts
    /// iterations, every action counts as emitted, and only gaps inside one burst feed
    /// the rate — a pause between activations is the user thinking, not the tool slow.
    #[test]
    fn the_counting_sink_reports_repetitions_and_a_burst_rate() {
        use sequencer_core::emit::{Emit, EmitAction, InputSink as _};
        use sequencer_core::input::Button;
        let mut inner = sequencer_input::MockInjector::new();
        let marker = EmitAction::ButtonDown(Button::Left);
        let mut sink = CountingSink::new(&mut inner, marker, Duration::from_millis(50));
        let click = |at_ms: u64, action: EmitAction| Emit {
            at: Timestamp::from_millis(at_ms),
            action,
            level: 0,
        };
        // Three clicks 50ms apart, a 10s pause, then two more.
        for at in [0, 50, 100, 10_000, 10_050] {
            sink.emit(&click(at, marker)).expect("counted");
            sink.emit(&click(at + 8, EmitAction::ButtonUp(Button::Left)))
                .expect("counted");
        }
        let summary = sink.finish(Duration::from_secs(11));
        assert_eq!(summary.iterations, 5);
        assert_eq!(summary.emitted, 10, "every action, not just the marker");
        assert_eq!(summary.paced_spans, 3, "the 10s pause is not a paced span");
        assert_eq!(summary.sent_cps().map(f64::round), Some(20.0));
        assert_eq!(summary.wall, Duration::from_secs(11));
    }

    /// A run whose marker never fires reports no rate rather than a fabricated one, still
    /// counts what it did send, and passes `release_all` straight through — the guard
    /// must reach the real sink.
    #[test]
    fn the_counting_sink_reports_nothing_it_did_not_see() {
        use sequencer_core::emit::{Emit, EmitAction, InputSink as _};
        use sequencer_core::input::{Button, Key};
        let mut inner = sequencer_input::MockInjector::new();
        let watcher = inner.clone();
        let mut sink = CountingSink::new(
            &mut inner,
            EmitAction::ButtonDown(Button::Left),
            Duration::from_millis(50),
        );
        sink.emit(&Emit {
            at: Timestamp::ZERO,
            action: EmitAction::KeyDown(Key::A),
            level: 0,
        })
        .expect("forwarded");
        sink.release_all();
        let summary = sink.finish(Duration::from_millis(5));
        assert_eq!(summary.iterations, 0);
        assert_eq!(
            summary.emitted, 1,
            "counted even though it was not the marker"
        );
        assert_eq!(summary.sent_cps(), None);
        assert_eq!(watcher.release_all_calls(), 1);
    }

    /// The window has to be forgiving of a machine stuttering, but nowhere near long enough
    /// to swallow a human pause between two deliberate presses.
    #[test]
    fn the_burst_window_tolerates_stutter_but_not_thinking() {
        // 20/s requested -> fuse of 80/s -> 12.5ms quarter-period -> 200ms window.
        let window = burst_gap(Duration::from_millis(50));
        assert!(
            window >= Duration::from_millis(150),
            "{window:?} is too strict for a stutter"
        );
        assert!(
            window <= Duration::from_millis(400),
            "{window:?} would swallow a real pause"
        );
    }
}
