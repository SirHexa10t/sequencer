//! The profile executor: trigger events in, synthetic presses out.
//!
//! Deliberately abstract: it sees a [`Bind`] list, an [`InputSink`], a [`Clock`] and
//! [`EventPump`]s, which is what makes every behaviour here — mirror edges, chord order,
//! gap-versus-WAIT, loops that stop on a re-press, RNG blocks, the release ledger —
//! assertable in tests with no display server. The X11 wiring in [`super::platform`]
//! only decides *which* sink and pumps those are.
//!
//! Sleeps go through the pump, not past it: a WAIT parks on [`EventPump::wait_until`],
//! so a re-press of the looping trigger or an emergency-stop key interrupts a pause
//! instead of queueing behind it. A `gate` (focus left the profile's program) is polled
//! alongside, so a running sequence stops rather than keep typing into whatever has
//! focus now. When the pump reports [`Wake::Interrupted`] (its source is gone), the
//! current iteration still completes on the bare clock — a half-done combo is exactly
//! the stuck state sequences must not leave — and then everything stops. The ledger
//! guarantees the exit invariant either way: whatever is down when the run ends is
//! released, in reverse order.

use sequencer_core::emit::{Emit, Holdable, InputSink};
use sequencer_core::input::{EventKind, Key, Mods};
use sequencer_core::rng::Rng;
use sequencer_core::time::{Clock, Duration};

use crate::runtime::{EventPump, Wake};
use crate::{Error, Result};

use super::{Action, Bind, Loops, Profile, Step};

/// How the run ended, for the caller that owns the outer loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The pump's source is gone (test script exhausted, capture stopped).
    SourceClosed,
    /// The emergency-stop key was pressed.
    EmergencyStop,
}

/// What a run listens to and answers to while executing.
///
/// One pump carries everything — every grab feeds the same queue, which is what lets
/// waits *block* instead of taking turns polling. `stop` is this profile's own
/// emergency chord as it arrives in an event: its primary key plus the modifier
/// classes that must be held with it (another profile's stop is not its business).
/// `gate` is the profile's licence to keep going: when it turns false mid-sequence —
/// focus left the program — the sequence stops rather than keep typing into whatever
/// is focused now. `mods_down` answers "are any of these modifier classes physically
/// held right now?" — the question a deferred tap waits on; without one (tests,
/// headless seams) taps fire immediately.
pub(super) struct Pumps<'a> {
    pub(super) triggers: &'a mut dyn EventPump,
    pub(super) stop: Option<(Key, Mods)>,
    pub(super) gate: Option<&'a dyn Fn() -> bool>,
    pub(super) mods_down: Option<&'a dyn Fn(Mods) -> bool>,
}

impl Pumps<'_> {
    /// Whether this event is the profile's own emergency chord.
    fn is_stop(&self, key: Key, mods: Mods) -> bool {
        self.stop == Some((key, mods))
    }
}

/// Runs `profile` until the pump closes or the emergency key is pressed. Releases
/// everything on the way out. `seed` feeds the RNG steps; a fixed seed replays a run.
pub(crate) fn run(
    profile: &Profile,
    sink: &mut dyn InputSink,
    clock: &dyn Clock,
    pump: &mut dyn EventPump,
    seed: u64,
) -> Result<Outcome> {
    let mut rng = Rng::new(seed);
    let mut held = Vec::new();
    let mut executor = Executor::new(profile, sink, clock, &mut rng, &mut held);
    let mut pumps = Pumps {
        triggers: pump,
        stop: stop_chord(profile),
        gate: None,
        mods_down: None,
    };
    let outcome = executor.event_loop(&mut pumps);
    // The exit invariant, on the error path too: nothing stays down because the run
    // ended. Reverse order, so a chorded modifier outlives what it modified.
    drain_held(sink, clock, &mut held);
    sink.release_all();
    outcome
}

/// A profile's emergency chord the way its event will arrive: primary plus held
/// modifier classes.
pub(super) fn stop_chord(profile: &Profile) -> Option<(Key, Mods)> {
    let chord = profile.emergency_stop.as_deref()?;
    Some((
        super::format::primary_key(chord)?,
        super::format::chord_mods(chord),
    ))
}

/// Releases a ledger in reverse — the exit invariant, callable by any loop owner. A
/// deactivated profile whose trigger key-ups will never arrive must not leave its
/// mirrors down, and a stopping run must not leave anything down at all.
pub(super) fn drain_held(sink: &mut dyn InputSink, clock: &dyn Clock, held: &mut Vec<Holdable>) {
    for target in held.drain(..).rev() {
        emit_infallible(sink, clock, target.up());
    }
    let _ = sink.flush();
}

/// One profile's execution view: the loop, the sequences and the sleeps share it.
///
/// `rng` and `held` are borrowed rather than owned so a manager running several
/// profiles can keep per-profile ledgers (a deactivating profile drains only its own
/// mirrors) and one shared dice cup, constructing a view per dispatched event.
pub(super) struct Executor<'a> {
    profile: &'a Profile,
    sink: &'a mut dyn InputSink,
    clock: &'a dyn Clock,
    rng: &'a mut Rng,
    held: &'a mut Vec<Holdable>,
    /// The pump reported [`Wake::Interrupted`]: finish the current iteration on the
    /// bare clock, start nothing new.
    pump_dead: bool,
}

/// With a gate to watch, waits are chunked so focus loss is noticed at least this often
/// — the gate is a poll, not an event, so it cannot itself wake the wait.
const CHUNK_NANOS: u64 = 50_000_000;

/// How often a deferred tap re-asks whether the trigger's modifiers are still held —
/// a poll, because after the ungrab their release events are routed elsewhere.
const DEFER_POLL_NANOS: u64 = 15_000_000;

/// What a wait-with-ears heard.
enum Heard {
    /// Nothing of note; the full duration passed.
    Nothing,
    /// The looping bind's own trigger went down again: stop its loop.
    StopBind,
    /// The profile's gate closed — focus left its program. Stop the sequence; the
    /// keys must not keep landing in whatever is focused now.
    GateLost,
    /// The emergency-stop key went down: stop the run.
    Emergency,
}

impl<'a> Executor<'a> {
    pub(super) fn new(
        profile: &'a Profile,
        sink: &'a mut dyn InputSink,
        clock: &'a dyn Clock,
        rng: &'a mut Rng,
        held: &'a mut Vec<Holdable>,
    ) -> Self {
        Self {
            profile,
            sink,
            clock,
            rng,
            held,
            pump_dead: false,
        }
    }

    fn event_loop(&mut self, pumps: &mut Pumps<'_>) -> Result<Outcome> {
        loop {
            match pumps.triggers.wait_until(None) {
                Wake::Event(event) => {
                    if let Some(outcome) = self.handle(event, pumps)? {
                        return Ok(outcome);
                    }
                }
                Wake::Deadline => {}
                Wake::Interrupted => return Ok(Outcome::SourceClosed),
            }
        }
    }

    /// Reacts to one trigger-pump event. `Some(outcome)` ends the whole run.
    pub(super) fn handle(
        &mut self,
        event: sequencer_core::input::InputEvent,
        pumps: &mut Pumps<'_>,
    ) -> Result<Option<Outcome>> {
        // Only presses drive anything now: a mirror tap releases its own keys, and the
        // trigger's release usually goes elsewhere anyway — ending our active grab
        // (see the capture backend) hands routing back to the server mid-press.
        if let EventKind::KeyDown(key) = event.kind {
            if pumps.is_stop(key, event.mods) {
                return Ok(Some(Outcome::EmergencyStop));
            }
            if let Some(bind) = bind_of(self.profile, key, event.mods) {
                match &bind.action {
                    Action::Mirror(targets) => {
                        let targets = targets.clone();
                        let tap = bind.tap;
                        // Modifiers the trigger holds but the target does not name
                        // would recolour the injected keys (a held shift turns `]`
                        // into `}`), so such a tap waits for the hand to leave them.
                        let held = super::format::chord_mods(&bind.trigger);
                        if !super::format::target_mods(&targets).covers(held) {
                            match self.await_mods_release(held, pumps)? {
                                Heard::Nothing => {}
                                Heard::StopBind | Heard::GateLost => return Ok(None),
                                Heard::Emergency => {
                                    return Ok(Some(Outcome::EmergencyStop));
                                }
                            }
                        }
                        tracing::info!(
                            trigger = %key,
                            target = %targets
                                .iter()
                                .map(|target| {
                                    sequencer_core::input::INPUT_MAP.display_name(*target)
                                })
                                .collect::<Vec<_>>()
                                .join(" "),
                            "mirror tap"
                        );
                        self.tap_chord(&targets, tap)?;
                    }
                    Action::Seq(steps) => {
                        let steps = steps.clone();
                        let bind = bind.clone();
                        return self.run_bind(&steps, &bind, pumps);
                    }
                }
            }
        }
        Ok(None)
    }

    /// Presses and releases a target chord — down in listed order, up in reverse —
    /// ledgered so an interrupted tap still releases.
    ///
    /// A mirror is a **tap**, not a held mirror, and X11 leaves no choice: ending our
    /// active grab is what lets the injected keys reach the rest of the session (see
    /// the capture backend), and once ended the trigger's own release is routed
    /// elsewhere — a held target would have nothing to release it. Holding the trigger
    /// still repeats the effect, because X auto-repeat re-presses it.
    fn tap_chord(&mut self, targets: &[Holdable], tap: Duration) -> Result<()> {
        for target in targets {
            emit(self.sink, self.clock, target.down())?;
            self.held.push(*target);
        }
        sleep(self.clock, tap);
        for target in targets.iter().rev() {
            emit(self.sink, self.clock, target.up())?;
            if let Some(position) = self.held.iter().rposition(|held| held == target) {
                self.held.remove(position);
            }
        }
        self.sink.flush().map_err(Error::from)
    }

    /// Runs a sequence bind: once, N times, or until its trigger is pressed again.
    ///
    /// Returns `Some(outcome)` when the whole run must end (emergency, or the pump died
    /// mid-sequence and the final iteration has been wound down).
    fn run_bind(
        &mut self,
        steps: &[Step],
        bind: &Bind,
        pumps: &mut Pumps<'_>,
    ) -> Result<Option<Outcome>> {
        let mut remaining = match bind.loops {
            Loops::Once => 1_u32,
            Loops::Times(times) => times,
            Loops::Infinite => u32::MAX,
        };
        let mut first = true;
        while remaining > 0 {
            if bind.loops != Loops::Infinite {
                remaining -= 1;
            }
            // Iterations are seams like any other: the bind's gap sits between them.
            if !first {
                match self.wait_with_ears(bind.gap, Some(bind), pumps)? {
                    Heard::Nothing => {}
                    Heard::StopBind | Heard::GateLost => break,
                    Heard::Emergency => return Ok(Some(Outcome::EmergencyStop)),
                }
            }
            first = false;
            match self.run_iteration(steps, bind, pumps)? {
                Heard::Nothing => {}
                Heard::StopBind | Heard::GateLost => break,
                Heard::Emergency => return Ok(Some(Outcome::EmergencyStop)),
            }
            // Every iteration consults the pump at least once after running, however
            // zero the bind's timings: an infinite loop with tap = 0 and gap = 0 would
            // otherwise spin forever with its stop key unheard.
            match self.poll_stop(bind, pumps)? {
                Heard::Nothing => {}
                Heard::StopBind | Heard::GateLost => break,
                Heard::Emergency => return Ok(Some(Outcome::EmergencyStop)),
            }
            if self.pump_dead && bind.loops == Loops::Infinite {
                // No more input can arrive, so no re-press can ever stop this loop:
                // wind down after the iteration that was owed. Finite counts keep
                // their promise on the bare clock instead.
                return Ok(Some(Outcome::SourceClosed));
            }
        }
        // A stopped loop lets go of what it pressed: the user asked it to stop, not to
        // leave ctrl down. Taps released their own; this drains the sequence's PRESSes.
        for target in self.held.drain(..).rev() {
            emit_infallible(self.sink, self.clock, target.up());
        }
        self.sink.flush().map_err(Error::from)?;
        Ok(None)
    }

    /// One pass over the steps. The seam rule from the template: a WAIT replaces the
    /// gap at its seam only, so a gap is inserted exactly between two non-WAIT
    /// neighbours; RNG markers are transparent to seams.
    fn run_iteration(
        &mut self,
        steps: &[Step],
        bind: &Bind,
        pumps: &mut Pumps<'_>,
    ) -> Result<Heard> {
        let mut wants_gap = false;
        let mut index = 0;
        while index < steps.len() {
            let step = &steps[index];
            index += 1;
            match step {
                Step::Wait(pause) => {
                    match self.wait_with_ears(*pause, Some(bind), pumps)? {
                        Heard::Nothing => {}
                        heard => return Ok(heard),
                    }
                    wants_gap = false;
                    continue;
                }
                // Chance blocks: roll, and on failure skip to the matching GNR. The
                // markers claim no gaps — they are control, not action.
                Step::Rng(chance) => {
                    if self.rng.unit() >= *chance {
                        index = skip_block(steps, index);
                    }
                    continue;
                }
                Step::RngEnd => continue,
                _ => {}
            }
            if wants_gap {
                match self.wait_with_ears(bind.gap, Some(bind), pumps)? {
                    Heard::Nothing => {}
                    heard => return Ok(heard),
                }
            }
            wants_gap = true;
            match step {
                Step::Tap(targets) => {
                    for target in targets {
                        emit(self.sink, self.clock, target.down())?;
                        self.held.push(*target);
                    }
                    match self.wait_with_ears(bind.tap, Some(bind), pumps)? {
                        Heard::Nothing => {}
                        heard => return Ok(heard),
                    }
                    for target in targets.iter().rev() {
                        emit(self.sink, self.clock, target.up())?;
                        if let Some(position) = self.held.iter().rposition(|h| h == target) {
                            self.held.remove(position);
                        }
                    }
                }
                Step::Hold(targets) => {
                    for target in targets {
                        emit(self.sink, self.clock, target.down())?;
                        self.held.push(*target);
                    }
                }
                Step::Release(targets) => {
                    for target in targets {
                        emit(self.sink, self.clock, target.up())?;
                        if let Some(position) = self.held.iter().rposition(|h| h == target) {
                            self.held.remove(position);
                        }
                    }
                }
                Step::Wait(_) | Step::Rng(_) | Step::RngEnd => unreachable!("handled above"),
            }
        }
        self.sink.flush().map_err(Error::from)?;
        Ok(Heard::Nothing)
    }

    /// Sleeps `duration` with the pump as the pillow: a re-press of `bind`'s trigger or
    /// an emergency press wakes it early with the news. Once the pump is dead, plain
    /// clock sleep — nothing can arrive, but the timing still owes its shape.
    fn wait_with_ears(
        &mut self,
        duration: Duration,
        bind: Option<&Bind>,
        pumps: &mut Pumps<'_>,
    ) -> Result<Heard> {
        if duration.is_zero() {
            return Ok(Heard::Nothing);
        }
        let nanos = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
        let deadline = self.clock.now().saturating_add_nanos(nanos);
        if self.pump_dead {
            self.clock.sleep_until(deadline);
            return Ok(Heard::Nothing);
        }
        // With a gate to watch, the wait is chunked so focus loss is noticed mid-pause
        // rather than after it; events still wake the wait instantly either way.
        loop {
            if let Some(gate) = pumps.gate
                && !gate()
            {
                return Ok(Heard::GateLost);
            }
            let now = self.clock.now();
            let chunk = if pumps.gate.is_some() {
                deadline.min(now.saturating_add_nanos(CHUNK_NANOS))
            } else {
                deadline
            };
            match pumps.triggers.wait_until(Some(chunk)) {
                Wake::Event(event) => {
                    if let EventKind::KeyDown(key) = event.kind {
                        if pumps.is_stop(key, event.mods) {
                            return Ok(Heard::Emergency);
                        }
                        // The grab fires a chord as its one ordinary key with the
                        // modifiers riding along as event state — so a re-press is
                        // the primary plus the chord's exact modifier classes.
                        if let Some(bind) = bind
                            && trigger_matches(bind, key, event.mods)
                        {
                            return Ok(Heard::StopBind);
                        }
                    }
                    // Any other event mid-sequence is dropped: two sequences cannot
                    // run at once in this executor, and queueing them for later has
                    // surprised every macro tool that tried it.
                    tracing::debug!("input during a sequence ignored");
                }
                Wake::Deadline => {
                    if self.clock.now() >= deadline {
                        return Ok(Heard::Nothing);
                    }
                }
                Wake::Interrupted => {
                    self.pump_dead = true;
                    self.clock.sleep_until(deadline);
                    return Ok(Heard::Nothing);
                }
            }
        }
    }

    /// Parks until none of `held`'s modifier classes is physically down, pumping all
    /// the while: the emergency chord still stops everything, a closed gate abandons
    /// the tap, and re-presses of the still-held trigger (X auto-repeat) are
    /// swallowed so one press queues one tap. Without a probe the wait is a no-op —
    /// better to fire recoloured than to never fire.
    fn await_mods_release(&mut self, held: Mods, pumps: &mut Pumps<'_>) -> Result<Heard> {
        let Some(mods_down) = pumps.mods_down else {
            return Ok(Heard::Nothing);
        };
        loop {
            if !mods_down(held) {
                return Ok(Heard::Nothing);
            }
            if let Some(gate) = pumps.gate
                && !gate()
            {
                return Ok(Heard::GateLost);
            }
            if self.pump_dead {
                return Ok(Heard::Nothing);
            }
            let deadline = self.clock.now().saturating_add_nanos(DEFER_POLL_NANOS);
            match pumps.triggers.wait_until(Some(deadline)) {
                Wake::Event(event) => {
                    if let EventKind::KeyDown(key) = event.kind
                        && pumps.is_stop(key, event.mods)
                    {
                        return Ok(Heard::Emergency);
                    }
                    tracing::debug!("input while waiting out held modifiers ignored");
                }
                Wake::Deadline => {}
                Wake::Interrupted => {
                    self.pump_dead = true;
                    return Ok(Heard::Nothing);
                }
            }
        }
    }

    /// Drains whatever the pumps already hold, without waiting: the zero-duration
    /// sibling of [`Executor::wait_with_ears`].
    fn poll_stop(&mut self, bind: &Bind, pumps: &mut Pumps<'_>) -> Result<Heard> {
        if let Some(gate) = pumps.gate
            && !gate()
        {
            return Ok(Heard::GateLost);
        }
        if self.pump_dead {
            return Ok(Heard::Nothing);
        }
        loop {
            match pumps.triggers.wait_until(Some(self.clock.now())) {
                Wake::Event(event) => {
                    if let EventKind::KeyDown(key) = event.kind {
                        if pumps.is_stop(key, event.mods) {
                            return Ok(Heard::Emergency);
                        }
                        if trigger_matches(bind, key, event.mods) {
                            return Ok(Heard::StopBind);
                        }
                    }
                    tracing::debug!("input during a sequence ignored");
                }
                Wake::Deadline => return Ok(Heard::Nothing),
                Wake::Interrupted => {
                    self.pump_dead = true;
                    return Ok(Heard::Nothing);
                }
            }
        }
    }
}

/// The index just past the GNR that closes the block opened before `from`.
fn skip_block(steps: &[Step], from: usize) -> usize {
    let mut depth = 1_u32;
    let mut index = from;
    while index < steps.len() {
        match steps[index] {
            Step::Rng(_) => depth += 1,
            Step::RngEnd => {
                depth -= 1;
                if depth == 0 {
                    return index + 1;
                }
            }
            _ => {}
        }
        index += 1;
    }
    // Validation pairs every RNG with a GNR; an unmatched one cannot get here.
    index
}

/// The binding an arriving key fires, if any.
///
/// A chord trigger arrives as its ordinary key — the grab already proved the modifiers
/// were held — so both shapes match on the same lookup. Validation guarantees no two
/// binds claim one key, so the first hit is the only hit.
fn bind_of(profile: &Profile, key: Key, mods: Mods) -> Option<&Bind> {
    profile
        .binds
        .iter()
        .find(|bind| trigger_matches(bind, key, mods))
}

/// Whether an event (primary key + held modifier classes) is exactly this bind's
/// trigger chord. Exact, not superset: `ctrl w` must not pass for `ctrl shift w`,
/// in either direction.
fn trigger_matches(bind: &Bind, key: Key, mods: Mods) -> bool {
    super::format::primary_key(&bind.trigger) == Some(key)
        && super::format::chord_mods(&bind.trigger) == mods
}

/// Sleeps `duration` on the clock. Used only by the mirror tap, which owes nothing to
/// events: it is a few milliseconds bracketing one press.
fn sleep(clock: &dyn Clock, duration: Duration) {
    if duration.is_zero() {
        return;
    }
    let nanos = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
    clock.sleep_until(clock.now().saturating_add_nanos(nanos));
}

/// One synthetic press or release, stamped now.
fn emit(
    sink: &mut dyn InputSink,
    clock: &dyn Clock,
    action: sequencer_core::emit::EmitAction,
) -> Result<()> {
    sink.emit(&Emit {
        at: clock.now(),
        action,
        level: 0,
    })
    .map_err(Error::from)
}

/// A release on a wind-down path, where a sink error must not stop the draining.
fn emit_infallible(
    sink: &mut dyn InputSink,
    clock: &dyn Clock,
    action: sequencer_core::emit::EmitAction,
) {
    let _ = sink.emit(&Emit {
        at: clock.now(),
        action,
        level: 0,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::ScriptedPump;
    use sequencer_core::emit::EmitAction;
    use sequencer_core::input::{InputEvent, Key};
    use sequencer_core::testutil::VirtualClock;
    use sequencer_core::time::Timestamp;
    use sequencer_input::MockInjector;

    fn press(key: Key) -> (Timestamp, InputEvent) {
        (
            Timestamp::ZERO,
            InputEvent::physical(Timestamp::ZERO, EventKind::KeyDown(key)),
        )
    }

    /// A press the way a chord's grab delivers it: primary key + held modifier
    /// classes as event state.
    fn press_with(key: Key, mods: Mods) -> (Timestamp, InputEvent) {
        (
            Timestamp::ZERO,
            InputEvent::physical(Timestamp::ZERO, EventKind::KeyDown(key)).with_mods(mods),
        )
    }

    fn release(key: Key) -> (Timestamp, InputEvent) {
        (
            Timestamp::ZERO,
            InputEvent::physical(Timestamp::ZERO, EventKind::KeyUp(key)),
        )
    }

    fn run_events(text: &str, events: Vec<(Timestamp, InputEvent)>) -> Vec<EmitAction> {
        let profile = super::super::parse(text).expect("profile should parse");
        let clock = VirtualClock::new();
        let mut sink = MockInjector::new();
        let watcher = sink.clone();
        let mut pump = ScriptedPump::new(events);
        run(&profile, &mut sink, &clock, &mut pump, 0).expect("run should succeed");
        watcher.recorded().iter().map(|e| e.action).collect()
    }

    /// A chord target taps as one gesture: members down in listed order, up in
    /// reverse, nothing outliving the press.
    #[test]
    fn a_chord_bind_taps_in_order_and_releases_in_reverse() {
        let actions = run_events(
            "[binds.\"rshift >\"]\nbind = \"shift ]\"",
            // The grab delivers the chord as its primary key + the held classes.
            vec![press_with(Key::Period, Mods::of_chord(&[Key::RightShift]))],
        );
        assert_eq!(
            actions,
            vec![
                EmitAction::KeyDown(Key::LeftShift),
                EmitAction::KeyDown(Key::RightBracket),
                EmitAction::KeyUp(Key::RightBracket),
                EmitAction::KeyUp(Key::LeftShift),
            ]
        );
    }

    /// A press taps the target: down then up, with nothing outliving the press.
    #[test]
    fn a_mirror_taps_its_target() {
        let actions = run_events(
            "[binds.PgUp]\nbind = \"volume-up\"",
            vec![press(Key::PageUp), release(Key::PageUp)],
        );
        assert_eq!(
            actions,
            vec![
                EmitAction::KeyDown(Key::VolumeUp),
                EmitAction::KeyUp(Key::VolumeUp)
            ]
        );
    }

    /// Each trigger press is one complete tap, so X auto-repeat (which arrives as
    /// extra downs) repeats the effect — holding PgUp keeps stepping the volume. It
    /// also means nothing is ever left held, which is what makes losing the trigger's
    /// release to the ungrab harmless.
    #[test]
    fn every_trigger_press_is_a_complete_tap() {
        let actions = run_events(
            "[binds.PgUp]\nbind = \"volume-up\"",
            vec![press(Key::PageUp), press(Key::PageUp), release(Key::PageUp)],
        );
        assert_eq!(
            actions,
            vec![
                EmitAction::KeyDown(Key::VolumeUp),
                EmitAction::KeyUp(Key::VolumeUp),
                EmitAction::KeyDown(Key::VolumeUp),
                EmitAction::KeyUp(Key::VolumeUp),
            ],
            "two presses, two taps, nothing left held"
        );
    }

    /// The combo from the template: chord order down, reverse order up, holds bracket.
    #[test]
    fn a_sequence_fires_in_order_with_reverse_chord_release() {
        let actions = run_events(
            "[binds.F6]\nseq = [\"PRESS ctrl\", \"space d\", \"RELEASE ctrl\"]",
            vec![press(Key::F6)],
        );
        assert_eq!(
            actions,
            vec![
                EmitAction::KeyDown(Key::LeftCtrl),
                EmitAction::KeyDown(Key::Space),
                EmitAction::KeyDown(Key::D),
                EmitAction::KeyUp(Key::D),
                EmitAction::KeyUp(Key::Space),
                EmitAction::KeyUp(Key::LeftCtrl),
            ]
        );
    }

    /// A trigger whose release never arrives — exactly what the ungrab causes — still
    /// leaves nothing pressed, because the tap released it on its own.
    #[test]
    fn a_trigger_release_that_never_comes_leaves_nothing_held() {
        let actions = run_events(
            "[binds.PgUp]\nbind = \"volume-up\"",
            vec![press(Key::PageUp)], // never released; the pump just ends
        );
        assert_eq!(
            actions,
            vec![
                EmitAction::KeyDown(Key::VolumeUp),
                EmitAction::KeyUp(Key::VolumeUp)
            ]
        );
    }

    /// An unbound key does nothing — the loop must not panic or emit.
    #[test]
    fn unbound_keys_are_ignored() {
        let actions = run_events("[binds.F6]\nseq = [\"a\"]", vec![press(Key::Q)]);
        assert!(actions.is_empty(), "{actions:?}");
    }

    /// `loop = 3` runs the sequence three times; the gap sits between iterations.
    #[test]
    fn a_finite_loop_repeats_the_sequence() {
        let actions = run_events(
            "[binds.F6]\ntap = \"0\"\ngap = \"0\"\nloop = 3\nseq = [\"a\"]",
            vec![press(Key::F6)],
        );
        let downs = actions
            .iter()
            .filter(|action| matches!(action, EmitAction::KeyDown(Key::A)))
            .count();
        assert_eq!(downs, 3, "{actions:?}");
    }

    /// Pressing the trigger again stops a loop and releases what it still pressed.
    #[test]
    fn a_repress_stops_a_loop_and_releases() {
        // The re-press arrives during the first iteration's WAIT; the loop must stop
        // and ctrl must come back up even though its RELEASE step was never reached.
        let actions = run_events(
            "[binds.F6]\nloop = \"inf\"\nseq = [\"PRESS ctrl\", \"WAIT 10s\", \"RELEASE ctrl\"]",
            vec![press(Key::F6), press(Key::F6)],
        );
        assert_eq!(
            actions,
            vec![
                EmitAction::KeyDown(Key::LeftCtrl),
                EmitAction::KeyUp(Key::LeftCtrl)
            ],
            "stop mid-wait, then release the pressed key"
        );
    }

    /// A chord-triggered loop is stopped the same way: the grab fires the chord as
    /// its primary key with the modifiers as event state, and the full chord must
    /// match. (Caught live: `[binds."ctrl shift f6"]` looped forever, the field's
    /// re-press falling into "input during a sequence ignored".)
    #[test]
    fn a_repress_stops_a_chord_triggered_loop_too() {
        let chord = Mods::of_chord(&[Key::LeftCtrl, Key::LeftShift]);
        let actions = run_events(
            "[binds.\"ctrl shift F6\"]\nloop = \"inf\"\nseq = [\"PRESS a\", \"WAIT 10s\", \"RELEASE a\"]",
            vec![press_with(Key::F6, chord), press_with(Key::F6, chord)],
        );
        assert_eq!(
            actions,
            vec![EmitAction::KeyDown(Key::A), EmitAction::KeyUp(Key::A)],
            "the primary + its modifier classes stop the chord's loop"
        );
    }

    /// The other half of full-chord matching: the primary alone, or with the wrong
    /// modifiers, is NOT the trigger — `ctrl w` must never pass for `ctrl shift w`.
    /// (Caught live: a `ctrl w` bind's press emergency-stopped a profile whose stop
    /// chord was `ctrl shift w`.)
    #[test]
    fn the_wrong_modifiers_do_not_fire_a_chord_bind() {
        let actions = run_events(
            "[binds.\"ctrl shift F6\"]\nbind = \"p\"",
            vec![
                press(Key::F6),
                press_with(Key::F6, Mods::of_chord(&[Key::LeftCtrl])),
            ],
        );
        assert!(
            actions.is_empty(),
            "bare F6 and ctrl+F6 are other chords entirely: {actions:?}"
        );
    }

    /// A chord mirror whose target does not name the held modifiers defers its tap
    /// until they are released — injecting under a held shift would recolour the
    /// target (`]` arrives as `}`). The scripted probe releases after three asks.
    #[test]
    fn a_contaminated_mirror_waits_for_the_modifiers_to_lift() {
        let profile = super::super::parse("[binds.\"shift b\"]\nbind = \"p\"").expect("parses");
        let clock = VirtualClock::new();
        let mut sink = MockInjector::new();
        let watcher = sink.clone();
        let mut rng = Rng::new(0);
        let mut held = Vec::new();
        let shift = Mods::of_chord(&[Key::LeftShift]);

        let asks = std::cell::Cell::new(0_u32);
        let mods_down = |mods: Mods| {
            assert_eq!(mods, shift, "the wait watches the trigger's own classes");
            asks.set(asks.get() + 1);
            asks.get() <= 3
        };
        // Inert releases keep the pump alive between polls — an exhausted pump reads
        // as "no more input can come" and would rightly fire the tap at once.
        let mut pump = ScriptedPump::new(vec![
            release(Key::A),
            release(Key::A),
            release(Key::A),
            release(Key::A),
        ]);
        let mut pumps = Pumps {
            triggers: &mut pump,
            stop: None,
            gate: None,
            mods_down: Some(&mods_down),
        };
        let mut executor = Executor::new(&profile, &mut sink, &clock, &mut rng, &mut held);
        let event =
            InputEvent::physical(Timestamp::ZERO, EventKind::KeyDown(Key::B)).with_mods(shift);
        executor.handle(event, &mut pumps).expect("runs");

        assert!(asks.get() > 3, "the tap waited out the held shift");
        let actions: Vec<EmitAction> = watcher.recorded().iter().map(|e| e.action).collect();
        assert_eq!(
            actions,
            vec![EmitAction::KeyDown(Key::P), EmitAction::KeyUp(Key::P)],
            "the tap fires clean once the modifiers lift"
        );
    }

    /// An infinite loop with no further input winds down after the iteration it owes:
    /// the pump is gone, so no re-press can ever arrive to stop it.
    #[test]
    fn an_infinite_loop_ends_when_the_source_closes() {
        let actions = run_events(
            "[binds.F6]\ntap = \"0\"\ngap = \"0\"\nloop = \"inf\"\nseq = [\"a\"]",
            vec![press(Key::F6)],
        );
        let downs = actions
            .iter()
            .filter(|action| matches!(action, EmitAction::KeyDown(Key::A)))
            .count();
        assert_eq!(downs, 1, "one owed iteration, then wind-down: {actions:?}");
    }

    /// Chance 1 always runs its block, chance 0 never does — the interval is half-open
    /// so the endpoints are exact, whatever the seed.
    #[test]
    fn rng_blocks_run_or_skip_at_the_exact_endpoints() {
        let text = "[binds.F6]\ntap = \"0\"\ngap = \"0\"\nseq = [\"RNG 1.0\", \"a\", \"GNR\", \"RNG 0%\", \"b\", \"GNR\", \"c\"]";
        let actions = run_events(text, vec![press(Key::F6)]);
        assert!(
            actions.contains(&EmitAction::KeyDown(Key::A)),
            "chance 1.0 must run: {actions:?}"
        );
        assert!(
            !actions.contains(&EmitAction::KeyDown(Key::B)),
            "chance 0 must skip: {actions:?}"
        );
        assert!(
            actions.contains(&EmitAction::KeyDown(Key::C)),
            "life continues after a skipped block: {actions:?}"
        );
    }

    /// A profile with a gate keeps running while the gate holds, and stops the moment
    /// it closes mid-sequence — releasing what it pressed, so keys never leak into
    /// whatever has focus once the program lost it.
    #[test]
    fn a_closed_gate_stops_a_running_sequence_and_releases() {
        let profile = super::super::parse(
            "[binds.F6]\nloop = \"inf\"\nseq = [\"PRESS ctrl\", \"WAIT 10s\", \"RELEASE ctrl\"]",
        )
        .expect("profile should parse");
        let clock = VirtualClock::new();
        let mut sink = MockInjector::new();
        let watcher = sink.clone();
        let mut rng = Rng::new(0);
        let mut held = Vec::new();
        let mut executor = Executor::new(&profile, &mut sink, &clock, &mut rng, &mut held);

        // The gate is open for the first event, then latched shut.
        let open = std::cell::Cell::new(true);
        let gate = || open.get();
        let mut pump = ScriptedPump::new(vec![press(Key::F6)]);
        let mut pumps = Pumps {
            triggers: &mut pump,
            stop: None,
            gate: Some(&gate),
            mods_down: None,
        };
        // First event starts the loop; it presses ctrl and parks on the 10s WAIT, where
        // the closed gate (set just before the wait re-checks) ends it.
        open.set(false);
        let event = InputEvent::physical(Timestamp::ZERO, EventKind::KeyDown(Key::F6));
        executor.handle(event, &mut pumps).expect("handles");
        drain_held(&mut sink, &clock, &mut held);

        let actions: Vec<EmitAction> = watcher.recorded().iter().map(|e| e.action).collect();
        assert_eq!(
            actions,
            vec![
                EmitAction::KeyDown(Key::LeftCtrl),
                EmitAction::KeyUp(Key::LeftCtrl)
            ],
            "gate loss releases the pressed key: {actions:?}"
        );
    }

    /// The emergency key ends the run immediately, even from inside a loop's WAIT, and
    /// everything pressed comes back up.
    #[test]
    fn the_emergency_key_stops_the_run_and_releases() {
        let profile = super::super::parse(
            "[defaults]\nemergency_stop = \"F8\"\n[binds.F6]\nloop = \"inf\"\nseq = [\"PRESS ctrl\", \"WAIT 10s\", \"RELEASE ctrl\"]",
        )
        .expect("profile should parse");
        let clock = VirtualClock::new();
        let mut sink = MockInjector::new();
        let watcher = sink.clone();
        let mut pump = ScriptedPump::new(vec![press(Key::F6), press(Key::F8)]);
        let outcome = run(&profile, &mut sink, &clock, &mut pump, 0).expect("run should succeed");
        assert_eq!(outcome, Outcome::EmergencyStop);
        let actions: Vec<EmitAction> = watcher.recorded().iter().map(|e| e.action).collect();
        assert_eq!(
            actions,
            vec![
                EmitAction::KeyDown(Key::LeftCtrl),
                EmitAction::KeyUp(Key::LeftCtrl)
            ],
            "emergency releases the pressed key on the way out"
        );
    }

    /// WAIT replaces the gap at its seam; elsewhere the gap applies. On a virtual
    /// clock the sleeps are visible as the timestamps of the surrounding emits.
    #[test]
    fn wait_replaces_the_gap_at_its_seam_only() {
        let profile = super::super::parse(
            "[binds.F6]\ntap = \"0\"\ngap = \"30ms\"\nseq = [\"a\", \"b\", \"WAIT 200ms\", \"c\"]",
        )
        .expect("profile should parse");
        let clock = VirtualClock::new();
        let mut sink = MockInjector::new();
        let watcher = sink.clone();
        let mut pump = ScriptedPump::new(vec![press(Key::F6)]);
        run(&profile, &mut sink, &clock, &mut pump, 0).expect("run should succeed");

        let downs: Vec<_> = watcher
            .recorded()
            .iter()
            .filter(|e| matches!(e.action, EmitAction::KeyDown(_)))
            .map(|e| e.at)
            .collect();
        // a at 0; gap before b -> 30ms; WAIT replaces the gap before c -> 230ms.
        assert_eq!(downs[0], Timestamp::ZERO);
        assert_eq!(downs[1], Timestamp::from_millis(30));
        assert_eq!(downs[2], Timestamp::from_millis(230));
    }
}
