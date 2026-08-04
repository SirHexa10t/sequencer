//! Test doubles, and a harness that drives the engine with them.
//!
//! Exported unconditionally rather than behind a feature, because the same doubles power
//! the CLI's tests and its `simulate` subcommand. A tool in this space lives or dies on
//! whether a user's "it does the wrong thing" can be turned into a reproducible case
//! without their hardware.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::fmt::Write as _;

use crate::emit::{Emit, EmitAction, EmitBuf, InputSink, SinkError};
use crate::engine::{Engine, TickStats};
use crate::input::InputEvent;
use crate::ir::Control;
use crate::time::{Clock, Duration, Timestamp};
use crate::validate::CompiledProfile;

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

/// The error a [`RecordingSink`] produces on demand.
#[derive(Debug, thiserror::Error)]
#[error("injected test failure")]
pub struct InjectedFailure;

/// A sink that records instead of acting.
#[derive(Debug, Default)]
pub struct RecordingSink {
    /// Everything emitted, in order.
    pub emitted: Vec<Emit>,
    /// Start failing once this many actions have been recorded.
    pub fail_after: Option<usize>,
    /// How many times [`InputSink::release_all`] was called.
    pub release_all_calls: u32,
    /// How many times [`InputSink::flush`] was called.
    pub flush_calls: u32,
}

impl RecordingSink {
    /// An empty recording sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The actions recorded, without their timestamps.
    #[must_use]
    pub fn actions(&self) -> Vec<EmitAction> {
        self.emitted.iter().map(|emit| emit.action).collect()
    }

    /// Anything pressed more often than it was released.
    ///
    /// The invariant that matters most in this whole crate: a stuck modifier key is the
    /// failure every comparable tool has shipped at least once.
    ///
    /// Deliberately one-sided. A surplus *release* — a program that lets go of a key it
    /// never took — is harmless, since releasing an unheld key is a no-op at the OS
    /// level, and a program is free to do it. A surplus *press* is the bug.
    #[must_use]
    pub fn leaked(&self) -> Vec<(crate::emit::Holdable, i64)> {
        let mut surplus = self.unbalanced();
        surplus.retain(|(_, count)| *count > 0);
        surplus
    }

    /// Whether nothing was left pressed.
    #[must_use]
    pub fn has_no_leaks(&self) -> bool {
        self.leaked().is_empty()
    }

    /// Every press/release count that did not cancel out, positive for surplus presses
    /// and negative for surplus releases.
    #[must_use]
    pub fn unbalanced(&self) -> Vec<(crate::emit::Holdable, i64)> {
        let mut tally: Vec<(crate::emit::Holdable, i64)> = Vec::new();
        for emit in &self.emitted {
            let (what, delta) = match (emit.action.holds(), emit.action.releases()) {
                (Some(what), _) => (what, 1),
                (_, Some(what)) => (what, -1),
                _ => continue,
            };
            match tally.iter_mut().find(|(seen, _)| *seen == what) {
                Some((_, count)) => *count += delta,
                None => tally.push((what, delta)),
            }
        }
        tally.retain(|(_, count)| *count != 0);
        tally
    }
}

impl InputSink for RecordingSink {
    fn emit(&mut self, emit: &Emit) -> Result<(), SinkError> {
        if self.fail_after == Some(self.emitted.len()) {
            return Err(SinkError::Backend(alloc::boxed::Box::new(InjectedFailure)));
        }
        self.emitted.push(*emit);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        self.flush_calls += 1;
        Ok(())
    }

    fn release_all(&mut self) {
        self.release_all_calls += 1;
    }
}

/// Engine, virtual clock and recording sink, wired together and driven.
///
/// Advances straight from one interesting instant to the next rather than stepping a
/// fixed tick, so a test covering ten minutes of clicking costs the same as one covering
/// ten milliseconds.
#[derive(Debug)]
pub struct Harness {
    engine: Engine,
    clock: VirtualClock,
    sink: RecordingSink,
    out: EmitBuf,
    pending: VecDeque<(Timestamp, InputEvent)>,
    /// Accumulated statistics across every tick so far.
    pub stats: TickStats,
    quit: bool,
}

impl Harness {
    /// Wires up a harness for `profile`, with jitter seeded to `seed`.
    #[must_use]
    pub fn new(profile: CompiledProfile, seed: u64) -> Self {
        Self {
            engine: Engine::new(profile, seed),
            clock: VirtualClock::new(),
            sink: RecordingSink::new(),
            out: EmitBuf::new(),
            pending: VecDeque::new(),
            stats: TickStats::default(),
            quit: false,
        }
    }

    /// The engine being driven.
    #[must_use]
    pub const fn engine(&self) -> &Engine {
        &self.engine
    }

    /// The clock, for adjusting overshoot or jumping time.
    #[must_use]
    pub const fn clock(&self) -> &VirtualClock {
        &self.clock
    }

    /// Everything emitted so far.
    #[must_use]
    pub const fn sink(&self) -> &RecordingSink {
        &self.sink
    }

    /// Whether a [`Control::Quit`] has been seen.
    #[must_use]
    pub const fn quit_requested(&self) -> bool {
        self.quit
    }

    /// Queues an input event for delivery at `at`.
    pub fn at(&mut self, at: Timestamp, event: InputEvent) -> &mut Self {
        self.pending.push_back((at, event));
        self
    }

    /// Queues an input event for delivery `ms` milliseconds after the epoch.
    pub fn at_ms(&mut self, ms: u64, kind: crate::input::EventKind) -> &mut Self {
        let at = Timestamp::from_millis(ms);
        self.at(at, InputEvent::physical(at, kind))
    }

    /// Ticks allowed in one [`Harness::run_until`] call before it gives up.
    ///
    /// A program that asks to be re-entered immediately, forever — a `Forever` loop with
    /// no `Wait` is the way to write one — would otherwise spin here until the test
    /// timeout, reporting nothing useful. The engine's step budget keeps the *runner*
    /// responsive in that situation; it cannot make the program terminate.
    pub const MAX_TICKS: u32 = 100_000;

    /// Runs until `until`, or until a quit control fires, or until nothing is left to do.
    ///
    /// # Panics
    ///
    /// If more than [`Harness::MAX_TICKS`] ticks pass without reaching `until`, which
    /// means the profile never yields.
    pub fn run_until(&mut self, until: Timestamp) {
        let mut ticks = 0_u32;
        while !self.quit {
            ticks += 1;
            assert!(
                ticks <= Self::MAX_TICKS,
                "ran {} ticks without reaching {until:?}; the profile never yields",
                Self::MAX_TICKS
            );
            let now = self.clock.now();
            if now > until {
                break;
            }
            self.deliver_due(now);
            let outcome = self.engine.tick(now, &mut self.out);
            self.drain_to_sink();
            self.accumulate(outcome.stats);
            if self.quit {
                break;
            }

            let next_event = self.pending.front().map(|(at, _)| *at);
            let Some(next) = soonest(outcome.next_deadline, next_event) else {
                break; // Idle, and nothing more is coming.
            };
            // Always move, so a deadline of `now` (the step budget ran out) cannot spin.
            let next = next.max(now.saturating_add_nanos(1));
            if next > until {
                break;
            }
            // Lands exactly on the deadline. A test that wants to simulate a machine
            // falling behind moves the clock itself, which keeps what is being tested
            // visible at the call site instead of hidden in a harness setting.
            self.clock.set(next);
        }
    }

    /// Runs until `ms` milliseconds after the epoch.
    pub fn run_until_ms(&mut self, ms: u64) {
        self.run_until(Timestamp::from_millis(ms));
    }

    /// Cancels everything and releases whatever is held.
    pub fn shutdown(&mut self) {
        self.engine.shutdown(self.clock.now(), &mut self.out);
        self.drain_to_sink();
    }

    fn deliver_due(&mut self, now: Timestamp) {
        while let Some(&(at, event)) = self.pending.front() {
            if at > now {
                break;
            }
            self.pending.pop_front();
            if self.engine.handle_input(event) == Some(Control::Quit) {
                self.quit = true;
            }
        }
    }

    fn drain_to_sink(&mut self) {
        for emit in self.out.as_slice() {
            // Ignoring the error is right here: a recording sink only fails when a test
            // asked it to, and the test is asserting on what got through.
            let _ = self.sink.emit(emit);
        }
        self.out.clear();
    }

    fn accumulate(&mut self, tick: TickStats) {
        self.stats.steps_run = self.stats.steps_run.saturating_add(tick.steps_run);
        self.stats.iterations_started = self
            .stats
            .iterations_started
            .saturating_add(tick.iterations_started);
        self.stats.iterations_completed = self
            .stats
            .iterations_completed
            .saturating_add(tick.iterations_completed);
        self.stats.slots_skipped = self.stats.slots_skipped.saturating_add(tick.slots_skipped);
        self.stats.budget_exhausted |= tick.budget_exhausted;
        self.stats.active = tick.active;
    }

    /// A compact, diffable rendering of what was emitted and when.
    ///
    /// Reads as `0 BD:left BU:left | 50 BD:left BU:left`. Worth the twenty lines: a
    /// failing cadence assertion that prints `.. | 100 .. | 153 ..` says "drift" at a
    /// glance, where a `Vec<Emit>` diff says nothing at all.
    #[must_use]
    pub fn timeline(&self) -> String {
        let mut out = String::new();
        let mut current: Option<Timestamp> = None;
        for emit in &self.sink.emitted {
            match current {
                // Same instant as the previous action: keep them on one group.
                Some(at) if at == emit.at => out.push(' '),
                _ => {
                    if current.is_some() {
                        out.push_str(" | ");
                    }
                    let _ = write!(out, "{} ", Millis(emit.at));
                    current = Some(emit.at);
                }
            }
            let _ = write!(out, "{}", Short(emit.action));
        }
        out
    }

    /// Panics unless the engine is holding nothing down.
    ///
    /// # Panics
    ///
    /// If any activation still holds a key or button, or if the emitted stream has an
    /// unmatched press.
    pub fn assert_quiescent(&self) {
        assert!(
            self.engine.is_quiescent(),
            "engine still holds inputs down; timeline: {}",
            self.timeline()
        );
        assert!(
            self.sink.has_no_leaks(),
            "these were pressed and never released: {:?}; timeline: {}",
            self.sink.leaked(),
            self.timeline()
        );
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Without this, a test that simply stops mid-click trips the executor's
        // drop-time assertion and reports a leak the test did not cause. Safe to run
        // while unwinding too, since shutdown only drains -- it never asserts.
        self.shutdown();
    }
}

/// The earlier of two optional instants.
fn soonest(a: Option<Timestamp>, b: Option<Timestamp>) -> Option<Timestamp> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (only, None) | (None, only) => only,
    }
}

/// Formats a timestamp as milliseconds, with sub-millisecond digits only when needed.
struct Millis(Timestamp);

impl core::fmt::Display for Millis {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let nanos = self.0.nanos();
        let ms = nanos / 1_000_000;
        match nanos % 1_000_000 {
            0 => write!(f, "{ms}"),
            // Six digits is the full sub-millisecond precision, but padding is not the same
            // as significance: half a millisecond is `.5`, not `.500000`. Trailing zeros are
            // divided off rather than trimmed from a string, since this crate has no
            // allocator to spare for a formatter.
            frac => {
                let (mut value, mut width) = (frac, 6_usize);
                while value % 10 == 0 {
                    value /= 10;
                    width -= 1;
                }
                write!(f, "{ms}.{value:0width$}")
            }
        }
    }
}

/// Formats an action in the shortest form that stays unambiguous.
struct Short(EmitAction);

impl core::fmt::Display for Short {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            EmitAction::KeyDown(key) => write!(f, "KD:{key}"),
            EmitAction::KeyUp(key) => write!(f, "KU:{key}"),
            EmitAction::ButtonDown(button) => write!(f, "BD:{button}"),
            EmitAction::ButtonUp(button) => write!(f, "BU:{button}"),
            EmitAction::Scroll { dx, dy } => write!(f, "SC:{dx},{dy}"),
            EmitAction::CursorTo { x, y } => write!(f, "CT:{x},{y}"),
            EmitAction::CursorBy { dx, dy } => write!(f, "CB:{dx},{dy}"),
        }
    }
}

#[cfg(test)]
mod millis_tests {
    use alloc::string::String;

    use super::{Millis, Timestamp};

    #[test]
    fn sub_millisecond_digits_appear_only_when_they_mean_something() {
        // No `ToString` without an allocator in scope here; the formatter is what is under
        // test anyway, so it is exercised through `write!` directly.
        let at = |nanos| {
            let mut rendered = String::new();
            core::fmt::write(&mut rendered, format_args!("{}", Millis(Timestamp::from_nanos(nanos))))
                .expect("writing to a String cannot fail");
            rendered
        };
        assert_eq!(at(0), "0");
        assert_eq!(at(50_000_000), "50", "a whole millisecond carries no fraction");
        assert_eq!(at(2_500_000), "2.5", "not 2.500000");
        assert_eq!(at(1_250_000), "1.25");
        assert_eq!(at(1_000_001), "1.000001", "real precision is still shown in full");
    }
}
