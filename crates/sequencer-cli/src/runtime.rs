//! The run loop: the only place in the project that sleeps or touches a sink.

use sequencer_core::emit::{EmitBuf, InputSink};
use sequencer_core::engine::{Engine, TickStats};
use sequencer_core::input::InputEvent;
use sequencer_core::ir::Control;
use sequencer_core::time::{Clock, Duration, Timestamp};
use sequencer_input::CaptureStream;

use crate::Error;

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

/// Caps the total rate of synthesized events *beyond what the profile asks for*.
///
/// Independent of the engine's own pacing, and cheap insurance: a bug that turns into a
/// runaway event storm can lock a desktop hard enough to need a switch to another virtual
/// terminal. Better to throttle and say so.
///
/// The limit is sized from the requested rate (see [`fuse_limit`]) rather than fixed, so
/// it never becomes a hidden ceiling on a deliberately fast run — the user asked for the
/// ceiling to be the machine, and a safety fuse that quietly re-imposed one would defeat
/// that.
#[derive(Debug)]
pub struct RateFuse {
    limit: u32,
    window_start: Timestamp,
    in_window: u32,
    tripped: bool,
}

impl RateFuse {
    /// A fuse allowing `limit` emits per second.
    #[must_use]
    pub const fn new(limit: u32) -> Self {
        Self {
            limit,
            window_start: Timestamp::ZERO,
            in_window: 0,
            tripped: false,
        }
    }

    /// Whether one more emit is allowed at `now`.
    pub fn allow(&mut self, now: Timestamp) -> bool {
        if now.saturating_sub(self.window_start) >= Duration::from_secs(1) {
            self.window_start = now;
            self.in_window = 0;
        }
        if self.in_window >= self.limit {
            if !self.tripped {
                self.tripped = true;
                tracing::warn!(
                    limit = self.limit,
                    "output rate limit reached; throttling. A profile asking for this \
                     much input can make a desktop unusable."
                );
            }
            return false;
        }
        self.in_window += 1;
        true
    }

    /// Whether the fuse has ever throttled.
    #[must_use]
    pub const fn tripped(&self) -> bool {
        self.tripped
    }
}

/// Floor for the fuse: even a slow profile may burst this many events per second before
/// being throttled, so cancellation drains and multi-action steps never trip it.
pub const DEFAULT_MAX_EMITS_PER_SEC: u32 = 20_000;

/// The fuse limit for a run that was asked for `cps` repetitions per second.
///
/// Four events per repetition of headroom (a click is two, a key tap is two plus its
/// release drain) on top of a generous floor. The fuse exists to stop *runaway* output,
/// not to second-guess a rate the user chose on purpose.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "explicitly clamped to the u32 range just below, and negatives return early"
)]
pub fn fuse_limit(cps: f64) -> u32 {
    if !cps.is_finite() || cps <= 0.0 {
        return DEFAULT_MAX_EMITS_PER_SEC;
    }
    // Saturates at u32::MAX ~ 4.3 billion events/s, which no machine reaches.
    let scaled = (cps * 4.0).min(f64::from(u32::MAX)) as u32;
    scaled.max(DEFAULT_MAX_EMITS_PER_SEC)
}

/// The longest gap between two repetitions that still counts as one continuous burst.
///
/// Four times the requested period, and at least a quarter-second. Generous on purpose: a
/// machine that stutters must not be mistaken for the user letting go. Still far shorter than
/// any real pause between two deliberate presses, which is the thing being excluded.
///
/// Derived from the *requested* cadence rather than from [`fuse_limit`], which floors at
/// [`DEFAULT_MAX_EMITS_PER_SEC`] and so tells you nothing about a rate below 5000/s.
fn burst_gap(cadence: Duration) -> Duration {
    (cadence * 4).max(Duration::from_millis(250))
}

/// Releases everything the sink holds when the loop leaves, however it leaves.
///
/// Owns the borrow rather than taking one alongside, because a guard that merely borrowed
/// would conflict with using the sink inside the loop. This is the layer that covers a
/// panic: unwinding runs `drop`, `drop` releases, and the user does not end up with a
/// modifier key stuck down. It is also why the release profile must not set
/// `panic = "abort"`.
struct ReleaseGuard<'a> {
    sink: &'a mut dyn InputSink,
}

impl Drop for ReleaseGuard<'_> {
    fn drop(&mut self) {
        self.sink.release_all();
    }
}

/// What a run did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunSummary {
    /// Actions delivered to the sink.
    pub emitted: u64,
    /// Actions dropped by the rate fuse.
    pub throttled: u64,
    /// Iterations begun.
    pub iterations: u64,
    /// Scheduled iterations dropped because the machine fell behind.
    pub slots_skipped: u64,
    /// Whether a quit control ended the run.
    pub quit: bool,
    /// How long the repeater was actually firing, summed across activations.
    ///
    /// Genuinely summed: the gap between two repetitions counts only when they belong to
    /// the same burst. Measuring first-to-last instead would fold every pause BETWEEN
    /// activations into the total — tap the trigger a few times over a minute and a perfect
    /// 20/s reads as 3/s, which looks like a broken tool rather than a user thinking.
    pub active: Duration,
    /// Gaps counted into [`Self::active`] — the denominator's matching numerator.
    ///
    /// Not `iterations - 1`: that counts the spans BETWEEN bursts too, which `active`
    /// deliberately excludes, and dividing one by the other would understate the rate by
    /// exactly the share of repetitions that started a burst.
    pub paced_spans: u64,
}

impl RunSummary {
    fn absorb(&mut self, stats: TickStats) {
        self.iterations += u64::from(stats.iterations_started);
        self.slots_skipped += u64::from(stats.slots_skipped);
    }

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

/// Drives an engine until it quits or its event source goes away.
///
/// # Errors
///
/// If the sink fails. The run still shuts the engine down and releases everything before
/// the error is returned.
pub fn run_engine(
    engine: &mut Engine,
    sink: &mut dyn InputSink,
    clock: &dyn Clock,
    pump: &mut dyn EventPump,
    max_emits_per_sec: u32,
    cadence: Duration,
) -> Result<RunSummary, Error> {
    let guard = ReleaseGuard { sink };
    let mut buf = EmitBuf::new();
    let mut fuse = RateFuse::new(max_emits_per_sec);
    let mut summary = RunSummary::default();
    let mut failure: Option<Error> = None;
    // Bracketing the repetitions rather than the whole run: time spent idle waiting for
    // the trigger is not time the machine failed to click in, and counting it would make
    // the achieved rate meaningless.
    // Burst accounting: the previous repetition's instant, and how far apart two of them
    // may be while still counting as the same burst.
    let mut previous_iteration: Option<Timestamp> = None;
    let same_burst = burst_gap(cadence);

    'run: loop {
        let now = clock.now();
        let outcome = engine.tick(now, &mut buf);
        summary.absorb(outcome.stats);
        if outcome.stats.iterations_started > 0 {
            if let Some(previous) = previous_iteration {
                let gap = now.saturating_sub(previous);
                if gap <= same_burst {
                    summary.active += gap;
                    summary.paced_spans += 1;
                }
            }
            previous_iteration = Some(now);
        }

        if !buf.is_empty() {
            for emit in buf.as_slice() {
                if fuse.allow(now) {
                    if let Err(err) = guard.sink.emit(emit) {
                        failure = Some(err.into());
                        break 'run;
                    }
                    summary.emitted += 1;
                } else {
                    summary.throttled += 1;
                }
            }
            if let Err(err) = guard.sink.flush() {
                failure = Some(err.into());
                break 'run;
            }
            buf.clear();
        }

        match pump.wait_until(outcome.next_deadline) {
            Wake::Event(event) => {
                if engine.handle_input(event) == Some(Control::Quit) {
                    summary.quit = true;
                    break 'run;
                }
            }
            Wake::Deadline => {}
            Wake::Interrupted => break 'run,
        }
    }

    // Always, on every path out including the failing one. A second error here would add
    // nothing to the first, so it is dropped rather than masking it.
    engine.shutdown(clock.now(), &mut buf);
    for emit in buf.as_slice() {
        let _ = guard.sink.emit(emit);
        summary.emitted += 1;
    }
    let _ = guard.sink.flush();

    match failure {
        Some(err) => Err(err),
        None => Ok(summary),
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

    /// The window has to be forgiving of a machine stuttering, but nowhere near long enough
    /// to swallow a human pause between two deliberate presses.
    #[test]
    fn the_burst_window_tolerates_stutter_but_not_thinking() {
        // 20/s requested -> fuse of 80/s -> 12.5ms quarter-period -> 200ms window.
        let window = burst_gap(Duration::from_millis(50));
        assert!(window >= Duration::from_millis(150), "{window:?} is too strict for a stutter");
        assert!(window <= Duration::from_millis(400), "{window:?} would swallow a real pause");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sequencer_core::CompiledProfile;
    use sequencer_core::clicker::ClickConfig;
    use sequencer_core::input::{EventKind, Key};
    use sequencer_core::testutil::VirtualClock;
    use sequencer_input::MockInjector;

    fn engine_for(config: ClickConfig) -> Engine {
        let profile = config.to_profile().expect("valid config");
        Engine::new(
            CompiledProfile::validate(profile).expect("valid profile"),
            0,
        )
    }

    #[test]
    fn a_quit_event_ends_the_run_and_releases() {
        let mut engine = engine_for(ClickConfig::new());
        let mut sink = MockInjector::new();
        let watcher = sink.clone();
        let clock = VirtualClock::new();
        let mut pump = ScriptedPump::new([(
            Timestamp::ZERO,
            InputEvent::physical(Timestamp::ZERO, EventKind::KeyDown(Key::F8)),
        )]);

        let summary = run_engine(
            &mut engine,
            &mut sink,
            &clock,
            &mut pump,
            DEFAULT_MAX_EMITS_PER_SEC,
            Duration::from_millis(50),
        )
        .expect("should run");

        assert!(summary.quit);
        assert_eq!(watcher.release_all_calls(), 1, "the drop guard must fire");
    }

    #[test]
    fn an_exhausted_pump_ends_the_run() {
        let mut engine = engine_for(ClickConfig::new());
        let mut sink = MockInjector::new();
        let clock = VirtualClock::new();
        let mut pump = ScriptedPump::new([]);

        let summary = run_engine(
            &mut engine,
            &mut sink,
            &clock,
            &mut pump,
            DEFAULT_MAX_EMITS_PER_SEC,
            Duration::from_millis(50),
        )
        .expect("should run");
        assert!(!summary.quit);
    }

    #[test]
    fn clicking_reaches_the_sink() {
        let mut engine = engine_for(ClickConfig::new());
        let mut sink = MockInjector::new();
        let watcher = sink.clone();
        // A virtual clock that never advances: the engine fires the first iteration at
        // t=0 and then waits, so the pump running dry is what ends the run.
        let clock = VirtualClock::new();
        let mut pump = ScriptedPump::new([(
            Timestamp::ZERO,
            InputEvent::physical(Timestamp::ZERO, EventKind::KeyDown(Key::F9)),
        )]);

        let summary = run_engine(
            &mut engine,
            &mut sink,
            &clock,
            &mut pump,
            DEFAULT_MAX_EMITS_PER_SEC,
            Duration::from_millis(50),
        )
        .expect("should run");

        assert!(summary.emitted >= 2, "expected a press and a release");
        assert!(!watcher.recorded().is_empty());
        assert_eq!(watcher.release_all_calls(), 1);
    }

    #[test]
    fn the_achieved_rate_measures_only_the_time_spent_repeating() {
        // 100 gaps measured across one second of firing is 100/s. The iteration count is no
        // longer the numerator — see `summary_tests` for why counting it overstated the
        // denominator whenever a run had more than one burst.
        let mut summary = RunSummary {
            iterations: 101,
            paced_spans: 100,
            active: Duration::from_secs(1),
            ..RunSummary::default()
        };
        assert!((summary.sent_cps().unwrap() - 100.0).abs() < 0.001);

        // Too little to say anything: better to report nothing than a made-up number.
        summary.paced_spans = 0;
        assert_eq!(summary.sent_cps(), None);
        summary.paced_spans = 50;
        summary.active = Duration::ZERO;
        assert_eq!(summary.sent_cps(), None);
    }

    #[test]
    fn the_fuse_scales_with_the_requested_rate_instead_of_capping_it() {
        // A safety fuse that quietly re-imposed a ceiling would defeat the whole point of
        // removing the artificial one.
        assert_eq!(fuse_limit(20.0), DEFAULT_MAX_EMITS_PER_SEC);
        assert_eq!(fuse_limit(100_000.0), 400_000);
        assert_eq!(fuse_limit(f64::from(u32::MAX)), u32::MAX);
        // Garbage rates never reach here (parse rejects them), but the fuse must still
        // answer sanely if they do.
        assert_eq!(fuse_limit(f64::NAN), DEFAULT_MAX_EMITS_PER_SEC);
        assert_eq!(fuse_limit(-1.0), DEFAULT_MAX_EMITS_PER_SEC);
    }

    #[test]
    fn the_fuse_throttles_and_says_so_once() {
        let mut fuse = RateFuse::new(2);
        let now = Timestamp::ZERO;
        assert!(fuse.allow(now));
        assert!(fuse.allow(now));
        assert!(!fuse.allow(now));
        assert!(fuse.tripped());

        // A new window resets the allowance but not the fact that it happened.
        let later = Timestamp::from_millis(1500);
        assert!(fuse.allow(later));
        assert!(fuse.tripped());
    }

    #[test]
    fn a_scripted_pump_delivers_in_order_then_stops() {
        let first = InputEvent::physical(Timestamp::ZERO, EventKind::KeyDown(Key::F9));
        let second = InputEvent::physical(Timestamp::from_millis(5), EventKind::KeyUp(Key::F9));
        let mut pump = ScriptedPump::new([
            (Timestamp::ZERO, first),
            (Timestamp::from_millis(5), second),
        ]);
        assert_eq!(pump.wait_until(None), Wake::Event(first));
        assert_eq!(pump.wait_until(None), Wake::Event(second));
        assert_eq!(pump.wait_until(None), Wake::Interrupted);
    }
}
