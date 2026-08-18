//! The profile executor: trigger events in, synthetic presses out.
//!
//! Deliberately abstract: it sees a [`Bind`] list, an [`InputSink`], a [`Clock`] and
//! [`EventPump`]s, which is what makes every behaviour here — mirror edges, chord order,
//! gap-versus-WAIT, loops that stop on a re-press, RNG blocks, the release ledger —
//! assertable in tests with no display server. The X11 wiring in [`super::manager`]
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

/// The lifted-tap seam's shape: one whole tap — targets, the trigger's held classes,
/// the trigger's own key when the target re-presses it, and the hold duration —
/// answering whether the backend could fire it.
pub(super) type LiftTap<'a> = &'a dyn Fn(&[Holdable], Mods, Option<Key>, Duration) -> bool;

/// What a run listens to and answers to while executing.
///
/// One pump carries everything — every grab feeds the same queue, which is what lets
/// waits *block* instead of taking turns polling. `stop` is this profile's own
/// emergency chords as they arrive in events: each a primary key plus the modifier
/// classes that must be held with it (another profile's stop is not its business).
/// `gate` is the profile's licence to keep going: when it turns false mid-sequence —
/// focus left the program — the sequence stops rather than keep typing into whatever
/// is focused now. `mods_down` answers "are any of these modifier classes physically
/// held right now?" — what decides, at fire time, which of a covered target's classes
/// the hand still supplies. `key_down` is the same question for one concrete key —
/// the trigger's own primary, whose release is unseeable after the ungrab — for the
/// tap that must re-press the very key the hand is still on. `lift` fires a whole
/// tap with the standing held keys lifted out of its way and pressed back after —
/// the path a tap takes when the hand's held classes would recolour it.
pub(super) struct Pumps<'a> {
    pub(super) triggers: &'a mut dyn EventPump,
    pub(super) stop: &'a [(Key, Mods)],
    pub(super) gate: Option<&'a dyn Fn() -> bool>,
    pub(super) mods_down: Option<&'a dyn Fn(Mods) -> bool>,
    pub(super) key_down: Option<&'a dyn Fn(Key) -> bool>,
    pub(super) lift: Option<LiftTap<'a>>,
}

impl Pumps<'_> {
    /// Whether this event is one of the profile's own emergency chords.
    fn is_stop(&self, key: Key, mods: Mods) -> bool {
        self.stop.contains(&(key, mods))
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
    let stops = stop_chords(profile);
    let mut executor = Executor::new(profile, sink, clock, &mut rng, &mut held);
    let mut pumps = Pumps {
        triggers: pump,
        stop: &stops,
        gate: None,
        mods_down: None,
        key_down: None,
        lift: None,
    };
    let outcome = executor.event_loop(&mut pumps);
    // The exit invariant, on the error path too: nothing stays down because the run
    // ended. Reverse order, so a chorded modifier outlives what it modified.
    drain_held(sink, clock, &mut held);
    sink.release_all();
    outcome
}

/// A profile's emergency chords the way their events will arrive: primary plus held
/// modifier classes, one entry per alternative spelling.
pub(super) fn stop_chords(profile: &Profile) -> Vec<(Key, Mods)> {
    profile
        .emergency_stop
        .iter()
        .filter_map(|chord| {
            Some((
                super::format::primary_key(chord)?,
                super::format::chord_mods(chord),
            ))
        })
        .collect()
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
            if let Some(bind) = bind_of(self.profile, key, event.mods, pumps.key_down) {
                match &bind.action {
                    Action::Mirror(targets) => {
                        let mut targets = targets.clone();
                        let tap = bind.tap;
                        let held = super::format::chord_mods(&bind.trigger);
                        let wanted = super::format::target_mods(&targets);
                        if wanted.covers(held) {
                            // A target that re-presses the trigger's own key cannot
                            // fire while the hand is still on it: the grab consumed a
                            // *press*, so the key is physically down, and a synthetic
                            // down for an already-down key makes no fresh edge for
                            // anyone (field bug: `ctrl w -> ctrl shift w` closed no
                            // terminal tab). Wait for the key to lift first.
                            if targets.contains(&Holdable::Key(key)) {
                                match self.await_key_release(key, pumps)? {
                                    Heard::Nothing => {}
                                    Heard::StopBind | Heard::GateLost => return Ok(None),
                                    Heard::Emergency => {
                                        return Ok(Some(Outcome::EmergencyStop));
                                    }
                                }
                            }
                            // Firing under the hand: classes the hand STILL supplies
                            // are not injected — our own down+up would end with a
                            // synthetic release that wipes the held state (logically
                            // up, physically down), and every following press of the
                            // chord would arrive modifier-less and miss the grab.
                            // Classes let go of during the wait are ours to press
                            // after all; without a probe the original hold is assumed.
                            let hand_supplies = |class: Mods| {
                                held.covers(class) && pumps.mods_down.is_none_or(|down| down(class))
                            };
                            targets.retain(|target| match target {
                                Holdable::Key(target_key) => Mods::of_key(*target_key)
                                    .is_none_or(|class| !hand_supplies(class)),
                                Holdable::Button(_) => true,
                            });
                        } else {
                            // Modifiers the trigger holds but the target does not
                            // name would recolour the tap (a held shift turns `]`
                            // into `}`), so the backend lifts the standing keys,
                            // taps clean, and presses them back — one call, one
                            // connection, immediately, on every press. When the
                            // target re-presses the trigger's own key, that key is
                            // lifted too: a fresh edge is impossible while it is
                            // physically down.
                            let lift_primary = targets.contains(&Holdable::Key(key)).then_some(key);
                            if let Some(lift) = pumps.lift
                                && lift(&targets, held, lift_primary, tap)
                            {
                                tracing::info!(
                                    trigger = %key,
                                    "mirror tap fired between lifted modifiers"
                                );
                                return Ok(None);
                            }
                            // No lifter (headless seams) or it could not fire:
                            // recoloured through the real sink — better than never.
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
    /// neighbours; RNG and LOOP markers are transparent to seams.
    fn run_iteration(
        &mut self,
        steps: &[Step],
        bind: &Bind,
        pumps: &mut Pumps<'_>,
    ) -> Result<Heard> {
        let mut wants_gap = false;
        let mut index = 0;
        // Open LOOP blocks: where each body starts, and how many runs it still owes.
        let mut loops: Vec<(usize, u32)> = Vec::new();
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
                Step::Loop(times) => {
                    loops.push((index, times - 1));
                    continue;
                }
                Step::LoopEnd => {
                    match loops.last_mut() {
                        Some((start, remaining)) if *remaining > 0 => {
                            *remaining -= 1;
                            index = *start;
                            // A block whose steps all cost zero time would spin with
                            // its stop keys unheard; every wrap consults the pump once,
                            // exactly as the bind-level loop does.
                            match self.poll_stop(bind, pumps)? {
                                Heard::Nothing => {}
                                heard => return Ok(heard),
                            }
                        }
                        Some(_) => {
                            loops.pop();
                        }
                        // Validation pairs every LOOP with a POOL; an unmatched one
                        // cannot get here.
                        None => {}
                    }
                    continue;
                }
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
                Step::Wait(_) | Step::Rng(_) | Step::RngEnd | Step::Loop(_) | Step::LoopEnd => {
                    unreachable!("handled above")
                }
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

    /// Parks until `key` is physically up — the wait a tap owes when its target
    /// re-presses the trigger's own key. The emergency chord still stops everything,
    /// a closed gate abandons the tap, and re-presses of the still-held trigger
    /// (X auto-repeat) are swallowed so one press queues one tap. (A poll, not a wait
    /// on events: after the ungrab the hand's releases are routed elsewhere and can
    /// never arrive here.) Without a probe it fires immediately: better a swallowed
    /// edge than a tap that never comes.
    fn await_key_release(&mut self, key: Key, pumps: &mut Pumps<'_>) -> Result<Heard> {
        let Some(key_down) = pumps.key_down else {
            return Ok(Heard::Nothing);
        };
        loop {
            if !key_down(key) {
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
                    if let EventKind::KeyDown(pressed) = event.kind
                        && pumps.is_stop(pressed, event.mods)
                    {
                        return Ok(Heard::Emergency);
                    }
                    tracing::debug!("input while a tap waits for the hand ignored");
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
/// A chord trigger arrives as its ordinary key — the grab already proved the modifier
/// CLASSES were held — but X folds left and right into one class, so spellings that
/// differ only by side share one grab and both match the folded event. When that
/// happens, the probe breaks the tie: the spelling whose sided modifier keys are all
/// physically down wins. A lone spelling keeps catching both sides, probe or none —
/// the probe is only consulted between twins.
fn bind_of<'p>(
    profile: &'p Profile,
    key: Key,
    mods: Mods,
    key_down: Option<&dyn Fn(Key) -> bool>,
) -> Option<&'p Bind> {
    let mut matching = profile
        .binds
        .iter()
        .filter(|bind| trigger_matches(bind, key, mods));
    let first = matching.next()?;
    let Some(second) = matching.next() else {
        return Some(first);
    };
    if let Some(down) = key_down {
        let hand_is_on = |bind: &&Bind| {
            bind.trigger
                .iter()
                .filter(|&&trigger_key| super::format::is_modifier(trigger_key))
                .all(|&trigger_key| down(trigger_key))
        };
        if let Some(bind) = [first, second].into_iter().chain(matching).find(hand_is_on) {
            return Some(bind);
        }
    }
    // No probe, or no side matched (a race with the release): the first spelling in
    // is the deterministic fallback.
    Some(first)
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
    use sequencer_core::input::{Button, InputEvent, Key};
    use sequencer_core::testutil::VirtualClock;
    use sequencer_core::time::Timestamp;
    use sequencer_input::MockInjector;

    /// What a recording lift seam collects: one entry per tap it was asked to fire.
    type LiftedCalls = std::cell::RefCell<Vec<(Vec<Holdable>, Mods, Option<Key>)>>;

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
            // The user's own shift supplies the target's shift: re-injecting it
            // would end with a synthetic release that wipes the held state.
            vec![press_with(Key::Period, Mods::of_chord(&[Key::RightShift]))],
        );
        assert_eq!(
            actions,
            vec![
                EmitAction::KeyDown(Key::RightBracket),
                EmitAction::KeyUp(Key::RightBracket),
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

    /// The inverse of contamination: the target names MORE modifiers than the trigger
    /// holds. The extra one (shift) is synthesized; the held one (ctrl) is the hand's
    /// own and must NOT be — a synthetic ctrl-release would strand the user's held
    /// ctrl logically-up, and the next press of the chord would miss the grab.
    /// (Field bug: `ctrl w -> ctrl shift w` worked once, then went dead until ctrl
    /// was re-pressed.)
    #[test]
    fn a_held_modifier_is_supplied_by_the_hand_not_reinjected() {
        let actions = run_events(
            "[binds.\"ctrl w\"]\nbind = \"ctrl shift w\"",
            vec![press_with(Key::W, Mods::of_chord(&[Key::LeftCtrl]))],
        );
        assert_eq!(
            actions,
            vec![
                EmitAction::KeyDown(Key::LeftShift),
                EmitAction::KeyDown(Key::W),
                EmitAction::KeyUp(Key::W),
                EmitAction::KeyUp(Key::LeftShift),
            ],
            "shift is the delta and gets synthesized; ctrl is held and must not be: {actions:?}"
        );
    }

    /// The template promises `ctrl w -> ctrl shift w` "keeps firing on every press" —
    /// but the grab fires while the hand's W is still physically down, and a
    /// synthesized down for an already-down key is swallowed by the server, so a tap
    /// fired immediately delivers shift with no W edge inside it. The tap must wait
    /// for the hand's W to lift, exactly as a contaminated mirror waits out held
    /// modifiers. (Field bug: with `[binds."ctrl w"] bind = "Ctrl Shift W"` running,
    /// ctrl+w closed no terminal tab while a physical ctrl+shift+w did.)
    #[test]
    fn a_mirror_that_represses_its_own_trigger_key_waits_for_the_key_to_lift() {
        let profile =
            super::super::parse("[binds.\"ctrl w\"]\nbind = \"ctrl shift w\"").expect("parses");
        let clock = VirtualClock::new();
        let mut sink = MockInjector::new();
        let watcher = sink.clone();
        let mut rng = Rng::new(0);
        let mut held = Vec::new();
        let ctrl = Mods::of_chord(&[Key::LeftCtrl]);

        // The hand holds W for three polls, then lifts it; ctrl stays down throughout
        // (covered by the target, so it is the hand's to supply either way).
        let asks = std::cell::Cell::new(0_u32);
        let key_down = |key: Key| {
            assert_eq!(key, Key::W, "the wait watches the trigger's own primary");
            asks.set(asks.get() + 1);
            asks.get() <= 3
        };
        let mods_down = |_: Mods| true; // ctrl never lifts
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
            stop: &[],
            gate: None,
            mods_down: Some(&mods_down),
            key_down: Some(&key_down),
            lift: None,
        };
        let mut executor = Executor::new(&profile, &mut sink, &clock, &mut rng, &mut held);
        let event =
            InputEvent::physical(Timestamp::ZERO, EventKind::KeyDown(Key::W)).with_mods(ctrl);
        executor.handle(event, &mut pumps).expect("runs");

        assert!(
            asks.get() > 3,
            "the tap must wait out the held W: fired while the key was still down, \
             its W edge is swallowed and the bind does nothing"
        );
        let actions: Vec<EmitAction> = watcher.recorded().iter().map(|e| e.action).collect();
        assert_eq!(
            actions,
            vec![
                EmitAction::KeyDown(Key::LeftShift),
                EmitAction::KeyDown(Key::W),
                EmitAction::KeyUp(Key::W),
                EmitAction::KeyUp(Key::LeftShift),
            ],
            "once W lifts: the delta (shift) plus a fresh W edge, ctrl still the hand's"
        );
    }

    /// Under the hand, only the primary needs a fresh edge: when the target's
    /// ordinary key differs from the trigger's, the delta fires immediately — shift
    /// and t are both up, so their injected edges are real edges.
    #[test]
    fn a_delta_with_a_different_primary_fires_immediately() {
        let actions = run_events(
            "[binds.\"ctrl w\"]\nbind = \"ctrl shift t\"",
            vec![press_with(Key::W, Mods::of_chord(&[Key::LeftCtrl]))],
        );
        assert_eq!(
            actions,
            vec![
                EmitAction::KeyDown(Key::LeftShift),
                EmitAction::KeyDown(Key::T),
                EmitAction::KeyUp(Key::T),
                EmitAction::KeyUp(Key::LeftShift),
            ]
        );
    }

    /// A button target rides the held modifiers: the trigger's ctrl is the hand's and
    /// is not re-injected, and a button has no modifier class to strip — ctrl+click.
    #[test]
    fn a_button_target_rides_the_held_modifiers() {
        let actions = run_events(
            "[binds.\"ctrl w\"]\nbind = \"ctrl mouse1\"",
            vec![press_with(Key::W, Mods::of_chord(&[Key::LeftCtrl]))],
        );
        assert_eq!(
            actions,
            vec![
                EmitAction::ButtonDown(Button::Left),
                EmitAction::ButtonUp(Button::Left)
            ]
        );
    }

    /// Without a lifter (tests, headless seams) a contaminated mirror fires at once
    /// through the plain sink — recoloured beats never. Pinned so the trade stays a
    /// decision, not an accident.
    #[test]
    fn without_a_lifter_a_contaminated_mirror_fires_at_once() {
        let actions = run_events(
            "[binds.\"shift b\"]\nbind = \"p\"",
            vec![press_with(Key::B, Mods::of_chord(&[Key::LeftShift]))],
        );
        assert_eq!(
            actions,
            vec![EmitAction::KeyDown(Key::P), EmitAction::KeyUp(Key::P)]
        );
    }

    /// A press carrying a subset of the stop chord's classes is not the stop:
    /// `ctrl e` must not end a run whose emergency is `ctrl shift e` — the stop's
    /// twin of full-chord trigger matching, exact in both directions.
    #[test]
    fn a_subset_of_the_stop_chord_does_not_stop_the_run() {
        let profile = super::super::parse(
            "[defaults]\nemergency_stop = \"ctrl shift e\"\n[binds.F6]\nbind = \"p\"",
        )
        .expect("parses");
        let clock = VirtualClock::new();
        let mut sink = MockInjector::new();
        let watcher = sink.clone();
        let mut pump = ScriptedPump::new(vec![
            press_with(Key::E, Mods::of_chord(&[Key::LeftCtrl])),
            press(Key::F6),
        ]);
        let outcome = run(&profile, &mut sink, &clock, &mut pump, 0).expect("runs");
        assert_eq!(outcome, Outcome::SourceClosed, "ctrl e is not ctrl shift e");
        let actions: Vec<EmitAction> = watcher.recorded().iter().map(|e| e.action).collect();
        assert_eq!(
            actions,
            vec![EmitAction::KeyDown(Key::P), EmitAction::KeyUp(Key::P)],
            "the run went on to serve the bind after the near-miss"
        );
    }

    /// A contaminated mirror — held classes the target does not name — goes through
    /// the lifted-tap backend: immediately, once per press, with the trigger's held
    /// classes handed over so the backend knows what to lift. No parking, no delay.
    #[test]
    fn a_contaminated_mirror_taps_between_lifted_modifiers() {
        let profile = super::super::parse("[binds.\"shift b\"]\nbind = \"p\"").expect("parses");
        let clock = VirtualClock::new();
        let mut sink = MockInjector::new();
        let watcher = sink.clone();
        let mut rng = Rng::new(0);
        let mut held = Vec::new();
        let shift = Mods::of_chord(&[Key::LeftShift]);

        let lifted: LiftedCalls = std::cell::RefCell::new(Vec::new());
        let lift = |targets: &[Holdable], held: Mods, primary: Option<Key>, _hold: Duration| {
            lifted.borrow_mut().push((targets.to_vec(), held, primary));
            true
        };
        // The pump is never consulted: lifted taps do not wait on anything.
        let mut pump = ScriptedPump::new(vec![]);
        let mut pumps = Pumps {
            triggers: &mut pump,
            stop: &[],
            gate: None,
            mods_down: None,
            key_down: None,
            lift: Some(&lift),
        };
        let mut executor = Executor::new(&profile, &mut sink, &clock, &mut rng, &mut held);
        for _ in 0..3 {
            let event =
                InputEvent::physical(Timestamp::ZERO, EventKind::KeyDown(Key::B)).with_mods(shift);
            executor.handle(event, &mut pumps).expect("runs");
        }

        let tap = (vec![Holdable::Key(Key::P)], Mods::SHIFT, None);
        assert_eq!(
            lifted.borrow().as_slice(),
            &[tap.clone(), tap.clone(), tap][..],
            "three presses, three lifted taps, no parking"
        );
        assert!(
            watcher.recorded().is_empty(),
            "a lifted tap must not touch the plain sink"
        );
    }

    /// The backend receives the trigger's full held classes, and — when the target
    /// re-presses the trigger's own key — that key too, so it can lift it for a
    /// fresh edge. Modifiers go back down afterwards; the primary never does.
    #[test]
    fn lifted_taps_name_the_held_classes_and_the_self_pressed_primary() {
        let lifted: LiftedCalls = std::cell::RefCell::new(Vec::new());
        let lift = |targets: &[Holdable], held: Mods, primary: Option<Key>, _hold: Duration| {
            lifted.borrow_mut().push((targets.to_vec(), held, primary));
            true
        };
        for (text, trigger, mods) in [
            (
                "[binds.\"ctrl shift b\"]\nbind = \"ctrl p\"",
                Key::B,
                Mods::of_chord(&[Key::LeftCtrl, Key::LeftShift]),
            ),
            (
                "[binds.\"shift w\"]\nbind = \"w\"",
                Key::W,
                Mods::of_chord(&[Key::LeftShift]),
            ),
        ] {
            let profile = super::super::parse(text).expect("parses");
            let clock = VirtualClock::new();
            let mut sink = MockInjector::new();
            let mut rng = Rng::new(0);
            let mut held = Vec::new();
            let mut pump = ScriptedPump::new(vec![]);
            let mut pumps = Pumps {
                triggers: &mut pump,
                stop: &[],
                gate: None,
                mods_down: None,
                key_down: None,
                lift: Some(&lift),
            };
            let mut executor = Executor::new(&profile, &mut sink, &clock, &mut rng, &mut held);
            let event =
                InputEvent::physical(Timestamp::ZERO, EventKind::KeyDown(trigger)).with_mods(mods);
            executor.handle(event, &mut pumps).expect("runs");
        }
        assert_eq!(
            lifted.borrow().as_slice(),
            &[
                (
                    vec![Holdable::Key(Key::LeftCtrl), Holdable::Key(Key::P)],
                    Mods::CTRL.and(Mods::SHIFT),
                    None,
                ),
                (vec![Holdable::Key(Key::W)], Mods::SHIFT, Some(Key::W)),
            ][..],
            "held classes ride along whole; the primary only when the target re-presses it"
        );
    }

    /// A lifter that cannot fire reports false, and the tap falls back to the plain
    /// sink — recoloured beats never.
    #[test]
    fn a_failed_lift_falls_back_to_the_real_sink() {
        let profile = super::super::parse("[binds.\"shift b\"]\nbind = \"p\"").expect("parses");
        let clock = VirtualClock::new();
        let mut sink = MockInjector::new();
        let watcher = sink.clone();
        let mut rng = Rng::new(0);
        let mut held = Vec::new();
        let shift = Mods::of_chord(&[Key::LeftShift]);

        let lift = |_: &[Holdable], _: Mods, _: Option<Key>, _: Duration| false;
        let mut pump = ScriptedPump::new(vec![]);
        let mut pumps = Pumps {
            triggers: &mut pump,
            stop: &[],
            gate: None,
            mods_down: None,
            key_down: None,
            lift: Some(&lift),
        };
        let mut executor = Executor::new(&profile, &mut sink, &clock, &mut rng, &mut held);
        let event =
            InputEvent::physical(Timestamp::ZERO, EventKind::KeyDown(Key::B)).with_mods(shift);
        executor.handle(event, &mut pumps).expect("runs");

        let actions: Vec<EmitAction> = watcher.recorded().iter().map(|e| e.action).collect();
        assert_eq!(
            actions,
            vec![EmitAction::KeyDown(Key::P), EmitAction::KeyUp(Key::P)],
            "no backend to lift with, so the plain sink fires recoloured"
        );
    }

    /// Side-variant spellings share one grab and the probe routes each press to the
    /// side the hand is on: the same folded event runs different binds — different
    /// mechanisms, even — depending on which shift is physically down.
    #[test]
    fn side_variant_triggers_route_by_the_held_side() {
        let text = "[binds.\"shift >\"]\nbind = \"shift ]\"\n\n[binds.\"rshift >\"]\nbind = \"q\"";

        // Right shift down: the rshift spelling wins — contaminated, so the lift
        // seam fires its clean `q` and the plain sink stays silent.
        {
            let profile = super::super::parse(text).expect("parses");
            let clock = VirtualClock::new();
            let mut sink = MockInjector::new();
            let watcher = sink.clone();
            let mut rng = Rng::new(0);
            let mut held = Vec::new();
            let lifted: LiftedCalls = std::cell::RefCell::new(Vec::new());
            let lift = |targets: &[Holdable], held: Mods, primary: Option<Key>, _hold: Duration| {
                lifted.borrow_mut().push((targets.to_vec(), held, primary));
                true
            };
            let key_down = |key: Key| key == Key::RightShift;
            let mut pump = ScriptedPump::new(vec![]);
            let mut pumps = Pumps {
                triggers: &mut pump,
                stop: &[],
                gate: None,
                mods_down: None,
                key_down: Some(&key_down),
                lift: Some(&lift),
            };
            let mut executor = Executor::new(&profile, &mut sink, &clock, &mut rng, &mut held);
            let event = InputEvent::physical(Timestamp::ZERO, EventKind::KeyDown(Key::Period))
                .with_mods(Mods::SHIFT);
            executor.handle(event, &mut pumps).expect("runs");
            assert_eq!(
                lifted.borrow().as_slice(),
                &[(vec![Holdable::Key(Key::Q)], Mods::SHIFT, None)][..],
                "the right-hand spelling ran, through the lift"
            );
            assert!(watcher.recorded().is_empty(), "and only through the lift");
        }

        // Left shift down: the generic spelling wins — covered, so the plain sink
        // taps the delta `]` under the hand's shift and the lift stays unused.
        {
            let profile = super::super::parse(text).expect("parses");
            let clock = VirtualClock::new();
            let mut sink = MockInjector::new();
            let watcher = sink.clone();
            let mut rng = Rng::new(0);
            let mut held = Vec::new();
            let lifted: LiftedCalls = std::cell::RefCell::new(Vec::new());
            let lift = |targets: &[Holdable], held: Mods, primary: Option<Key>, _hold: Duration| {
                lifted.borrow_mut().push((targets.to_vec(), held, primary));
                true
            };
            let key_down = |key: Key| key == Key::LeftShift;
            let mut pump = ScriptedPump::new(vec![]);
            let mut pumps = Pumps {
                triggers: &mut pump,
                stop: &[],
                gate: None,
                mods_down: None,
                key_down: Some(&key_down),
                lift: Some(&lift),
            };
            let mut executor = Executor::new(&profile, &mut sink, &clock, &mut rng, &mut held);
            let event = InputEvent::physical(Timestamp::ZERO, EventKind::KeyDown(Key::Period))
                .with_mods(Mods::SHIFT);
            executor.handle(event, &mut pumps).expect("runs");
            let actions: Vec<EmitAction> = watcher.recorded().iter().map(|e| e.action).collect();
            assert_eq!(
                actions,
                vec![
                    EmitAction::KeyDown(Key::RightBracket),
                    EmitAction::KeyUp(Key::RightBracket)
                ],
                "the left-hand spelling ran: the hand's shift recolours `]` into the \
                 wanted `}}`, only the delta is injected"
            );
            assert!(lifted.borrow().is_empty(), "nothing needed lifting");
        }
    }

    /// A lone spelling keeps catching both sides — the probe is only consulted to
    /// break ties between side twins.
    #[test]
    fn a_lone_spelling_catches_either_shift() {
        let profile = super::super::parse("[binds.\"shift b\"]\nbind = \"p\"").expect("parses");
        let clock = VirtualClock::new();
        let mut sink = MockInjector::new();
        let mut rng = Rng::new(0);
        let mut held = Vec::new();
        let lifted: LiftedCalls = std::cell::RefCell::new(Vec::new());
        let lift = |targets: &[Holdable], held: Mods, primary: Option<Key>, _hold: Duration| {
            lifted.borrow_mut().push((targets.to_vec(), held, primary));
            true
        };
        // The spelling parses as the LEFT shift; the hand is on the RIGHT one.
        let key_down = |key: Key| key == Key::RightShift;
        let mut pump = ScriptedPump::new(vec![]);
        let mut pumps = Pumps {
            triggers: &mut pump,
            stop: &[],
            gate: None,
            mods_down: None,
            key_down: Some(&key_down),
            lift: Some(&lift),
        };
        let mut executor = Executor::new(&profile, &mut sink, &clock, &mut rng, &mut held);
        let event = InputEvent::physical(Timestamp::ZERO, EventKind::KeyDown(Key::B))
            .with_mods(Mods::SHIFT);
        executor.handle(event, &mut pumps).expect("runs");
        assert_eq!(
            lifted.borrow().len(),
            1,
            "no twin, no side check: the lone spelling fires whichever shift is down"
        );
    }

    /// If the hand lets go of the whole chord during the wait, the tap synthesizes
    /// what the hand no longer supplies — the full target, ctrl included.
    #[test]
    fn a_chord_released_during_the_wait_is_synthesized_whole() {
        let profile =
            super::super::parse("[binds.\"ctrl w\"]\nbind = \"ctrl shift w\"").expect("parses");
        let clock = VirtualClock::new();
        let mut sink = MockInjector::new();
        let watcher = sink.clone();
        let mut rng = Rng::new(0);
        let mut held = Vec::new();

        // One poll finds W still down; by the next, everything is up — ctrl included.
        let asks = std::cell::Cell::new(0_u32);
        let key_down = |_: Key| {
            asks.set(asks.get() + 1);
            asks.get() <= 1
        };
        let mods_down = |_: Mods| false;
        let mut pump = ScriptedPump::new(vec![release(Key::A), release(Key::A)]);
        let mut pumps = Pumps {
            triggers: &mut pump,
            stop: &[],
            gate: None,
            mods_down: Some(&mods_down),
            key_down: Some(&key_down),
            lift: None,
        };
        let mut executor = Executor::new(&profile, &mut sink, &clock, &mut rng, &mut held);
        let event = InputEvent::physical(Timestamp::ZERO, EventKind::KeyDown(Key::W))
            .with_mods(Mods::of_chord(&[Key::LeftCtrl]));
        executor.handle(event, &mut pumps).expect("runs");

        let actions: Vec<EmitAction> = watcher.recorded().iter().map(|e| e.action).collect();
        assert_eq!(
            actions,
            vec![
                EmitAction::KeyDown(Key::LeftCtrl),
                EmitAction::KeyDown(Key::LeftShift),
                EmitAction::KeyDown(Key::W),
                EmitAction::KeyUp(Key::W),
                EmitAction::KeyUp(Key::LeftShift),
                EmitAction::KeyUp(Key::LeftCtrl),
            ],
            "the hand supplies nothing anymore, so everything is injected"
        );
    }

    /// `also` spellings are full binds: each trigger fires the same action.
    #[test]
    fn an_also_spelling_fires_the_same_action() {
        let actions = run_events(
            "[binds.F6]\nalso = [\"F7\"]\nbind = \"p\"",
            vec![press(Key::F6), press(Key::F7)],
        );
        assert_eq!(
            actions,
            vec![
                EmitAction::KeyDown(Key::P),
                EmitAction::KeyUp(Key::P),
                EmitAction::KeyDown(Key::P),
                EmitAction::KeyUp(Key::P),
            ]
        );
    }

    /// Any spelling in an emergency_stop list ends the run.
    #[test]
    fn any_stop_alternative_ends_the_run() {
        let profile = super::super::parse(
            "[defaults]\nemergency_stop = [\"F8\", \"ctrl shift e\"]\n[binds.F6]\nbind = \"p\"",
        )
        .expect("parses");
        let clock = VirtualClock::new();
        let mut sink = MockInjector::new();
        let mut pump = ScriptedPump::new(vec![press_with(
            Key::E,
            Mods::of_chord(&[Key::LeftCtrl, Key::LeftShift]),
        )]);
        let outcome = run(&profile, &mut sink, &clock, &mut pump, 0).expect("runs");
        assert_eq!(outcome, Outcome::EmergencyStop);
    }

    /// LOOP/POOL repeat their block, then life continues — the bookmark-cleanup
    /// shape: five passes over the block, one `ctrl w` after.
    #[test]
    fn loop_blocks_repeat_their_steps() {
        let actions = run_events(
            "[binds.F6]\ntap = \"0\"\ngap = \"0\"\nseq = [\"LOOP 5\", \"a\", \"POOL\", \"b\"]",
            vec![press(Key::F6)],
        );
        let downs_a = actions
            .iter()
            .filter(|action| matches!(action, EmitAction::KeyDown(Key::A)))
            .count();
        assert_eq!(downs_a, 5, "{actions:?}");
        assert_eq!(
            actions.last(),
            Some(&EmitAction::KeyUp(Key::B)),
            "the step after POOL runs once, after the block: {actions:?}"
        );
    }

    /// Blocks nest: an inner LOOP multiplies the outer one, and an RNG inside rolls
    /// once per pass — its exact endpoints stay exact.
    #[test]
    fn loop_blocks_nest_with_each_other_and_with_rng() {
        let actions = run_events(
            "[binds.F6]\ntap = \"0\"\ngap = \"0\"\nseq = [\"LOOP 2\", \"LOOP 3\", \"a\", \"POOL\", \"RNG 0%\", \"b\", \"GNR\", \"POOL\", \"c\"]",
            vec![press(Key::F6)],
        );
        let count = |key: Key| {
            actions
                .iter()
                .filter(|action| matches!(action, EmitAction::KeyDown(k) if *k == key))
                .count()
        };
        assert_eq!(count(Key::A), 6, "2 outer x 3 inner: {actions:?}");
        assert_eq!(
            count(Key::B),
            0,
            "chance 0 skips in every pass: {actions:?}"
        );
        assert_eq!(count(Key::C), 1, "{actions:?}");
    }

    /// LOOP iterations are seams like any other: the gap sits between them, and the
    /// markers themselves claim none.
    #[test]
    fn the_gap_sits_between_loop_iterations() {
        let profile = super::super::parse(
            "[binds.F6]\ntap = \"0\"\ngap = \"30ms\"\nseq = [\"LOOP 2\", \"a\", \"POOL\", \"b\"]",
        )
        .expect("parses");
        let clock = VirtualClock::new();
        let mut sink = MockInjector::new();
        let watcher = sink.clone();
        let mut pump = ScriptedPump::new(vec![press(Key::F6)]);
        run(&profile, &mut sink, &clock, &mut pump, 0).expect("runs");

        let downs: Vec<_> = watcher
            .recorded()
            .iter()
            .filter(|e| matches!(e.action, EmitAction::KeyDown(_)))
            .map(|e| e.at)
            .collect();
        // a at 0; gap between iterations -> 30ms; gap before b -> 60ms.
        assert_eq!(downs[0], Timestamp::ZERO);
        assert_eq!(downs[1], Timestamp::from_millis(30));
        assert_eq!(downs[2], Timestamp::from_millis(60));
    }

    /// A re-press stops the bind from inside a LOOP block like anywhere else,
    /// releasing what the iteration still held.
    #[test]
    fn a_repress_stops_inside_a_loop_block_and_releases() {
        let actions = run_events(
            "[binds.F6]\nseq = [\"LOOP 3\", \"PRESS ctrl\", \"WAIT 10s\", \"RELEASE ctrl\", \"POOL\"]",
            vec![press(Key::F6), press(Key::F6)],
        );
        assert_eq!(
            actions,
            vec![
                EmitAction::KeyDown(Key::LeftCtrl),
                EmitAction::KeyUp(Key::LeftCtrl)
            ],
            "stop mid-wait inside the block, then release: {actions:?}"
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
            stop: &[],
            gate: Some(&gate),
            mods_down: None,
            key_down: None,
            lift: None,
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
