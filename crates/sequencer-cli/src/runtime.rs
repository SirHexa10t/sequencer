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
    /// Only the time between the first and last repetition, not the whole run, so idle
    /// time waiting for the trigger does not drag the measured rate down.
    pub active: Duration,
}

impl RunSummary {
    fn absorb(&mut self, stats: TickStats) {
        self.iterations += u64::from(stats.iterations_started);
        self.slots_skipped += u64::from(stats.slots_skipped);
    }

    /// Repetitions per second actually achieved, if enough of them happened to say.
    ///
    /// The number that matters when the requested rate is near what the machine can do:
    /// "you asked for 5000 and got 3200" is the answer, and a rate the tool silently
    /// failed to hit would otherwise look like success.
    #[must_use]
    pub fn achieved_cps(&self) -> Option<f64> {
        let seconds = self.active.as_secs_f64();
        (self.iterations > 1 && seconds > 0.0).then(|| {
            // `iterations - 1` because N repetitions span N-1 gaps.
            #[allow(
                clippy::cast_precision_loss,
                reason = "a count this large would need centuries of clicking"
            )]
            let spans = (self.iterations - 1) as f64;
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
) -> Result<RunSummary, Error> {
    let guard = ReleaseGuard { sink };
    let mut buf = EmitBuf::new();
    let mut fuse = RateFuse::new(max_emits_per_sec);
    let mut summary = RunSummary::default();
    let mut failure: Option<Error> = None;
    // Bracketing the repetitions rather than the whole run: time spent idle waiting for
    // the trigger is not time the machine failed to click in, and counting it would make
    // the achieved rate meaningless.
    let mut first_iteration: Option<Timestamp> = None;
    let mut last_iteration = Timestamp::ZERO;

    'run: loop {
        let now = clock.now();
        let outcome = engine.tick(now, &mut buf);
        summary.absorb(outcome.stats);
        if outcome.stats.iterations_started > 0 {
            first_iteration.get_or_insert(now);
            last_iteration = now;
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

    summary.active =
        first_iteration.map_or(Duration::ZERO, |start| last_iteration.saturating_sub(start));

    match failure {
        Some(err) => Err(err),
        None => Ok(summary),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sequencer_core::CompiledProfile;
    use sequencer_core::click::ClickConfig;
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
        )
        .expect("should run");

        assert!(summary.emitted >= 2, "expected a press and a release");
        assert!(!watcher.recorded().is_empty());
        assert_eq!(watcher.release_all_calls(), 1);
    }

    #[test]
    fn the_achieved_rate_measures_only_the_time_spent_repeating() {
        let mut summary = RunSummary {
            iterations: 101,
            active: Duration::from_secs(1),
            ..RunSummary::default()
        };
        // 101 repetitions span 100 gaps, so one second of them is 100/s, not 101/s.
        assert!((summary.achieved_cps().unwrap() - 100.0).abs() < 0.001);

        // Too little to say anything: better to report nothing than a made-up number.
        summary.iterations = 1;
        assert_eq!(summary.achieved_cps(), None);
        summary.iterations = 50;
        summary.active = Duration::ZERO;
        assert_eq!(summary.achieved_cps(), None);
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
