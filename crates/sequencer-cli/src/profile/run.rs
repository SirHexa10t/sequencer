//! The profile executor: trigger events in, synthetic presses out.
//!
//! Deliberately small and deliberately abstract: it sees a [`Bind`] list, an
//! [`InputSink`], a [`Clock`] and an [`EventPump`], which is what makes every behaviour
//! here — mirror edges, chord order, gap-versus-WAIT, the release ledger — assertable in
//! tests with no display server. The X11 wiring in [`super::platform`] only decides
//! *which* sink and pump those are.
//!
//! A sequence runs to completion once triggered: steps sleep on the clock inline. That
//! is a scope choice, not an accident — profiles are short combos, and suspending them
//! mid-flight would need the engine's tick model (where this logic will eventually
//! move). The ledger still guarantees the exit invariant: whatever was down when the
//! pump ends is released, in reverse order, before this returns.

use sequencer_core::emit::{Emit, Holdable, InputSink};
use sequencer_core::input::EventKind;
use sequencer_core::time::{Clock, Duration};

use crate::runtime::{EventPump, Wake};
use crate::{Error, Result};

use super::{Action, Bind, Profile, Step};

/// Runs `profile` until the pump is interrupted. Releases everything on the way out.
pub(crate) fn run(
    profile: &Profile,
    sink: &mut dyn InputSink,
    clock: &dyn Clock,
    pump: &mut dyn EventPump,
) -> Result<()> {
    let mut held: Vec<Holdable> = Vec::new();
    let outcome = event_loop(profile, sink, clock, pump, &mut held);
    // The exit invariant, on the error path too: nothing stays down because the run
    // ended. Reverse order, so a chorded modifier outlives what it modified.
    for target in held.drain(..).rev() {
        let _ = emit(sink, clock, target.up());
    }
    sink.release_all();
    outcome
}

/// The loop proper. `held` lives outside so the caller can drain it on any exit.
fn event_loop(
    profile: &Profile,
    sink: &mut dyn InputSink,
    clock: &dyn Clock,
    pump: &mut dyn EventPump,
    held: &mut Vec<Holdable>,
) -> Result<()> {
    loop {
        match pump.wait_until(None) {
            Wake::Event(event) => match event.kind {
                EventKind::KeyDown(key) => {
                    if let Some(bind) = bind_of(profile, key) {
                        match &bind.action {
                            Action::Mirror(target) => {
                                // Repeats arrive as extra downs while held; a target
                                // already down stays down rather than double-pressing.
                                if !held.contains(target) {
                                    emit(sink, clock, target.down())?;
                                    held.push(*target);
                                }
                            }
                            Action::Seq(steps) => run_seq(steps, bind, sink, clock, held)?,
                        }
                    }
                }
                EventKind::KeyUp(key) => {
                    if let Some(bind) = bind_of(profile, key)
                        && let Action::Mirror(target) = &bind.action
                        && let Some(position) = held.iter().rposition(|h| h == target)
                    {
                        emit(sink, clock, target.up())?;
                        held.remove(position);
                    }
                }
                _ => {}
            },
            Wake::Deadline => {}
            Wake::Interrupted => return Ok(()),
        }
    }
}

/// The binding a single-key trigger fires, if any. Chord triggers never match here —
/// the platform layer refuses to run them until a backend can hear one.
fn bind_of(profile: &Profile, key: sequencer_core::input::Key) -> Option<&Bind> {
    profile
        .binds
        .iter()
        .find(|bind| bind.trigger.as_slice() == [key])
}

/// Fires one sequence: taps, holds, releases and pauses, with the bind's `gap` between
/// consecutive steps wherever no WAIT replaces it.
fn run_seq(
    steps: &[Step],
    bind: &Bind,
    sink: &mut dyn InputSink,
    clock: &dyn Clock,
    held: &mut Vec<Holdable>,
) -> Result<()> {
    // The seam rule from the template: a WAIT replaces the gap at its seam only, so a
    // gap is inserted exactly between two non-WAIT neighbours.
    let mut wants_gap = false;
    for step in steps {
        if let Step::Wait(pause) = step {
            sleep(clock, *pause);
            wants_gap = false;
            continue;
        }
        if wants_gap {
            sleep(clock, bind.gap);
        }
        wants_gap = true;
        match step {
            Step::Tap(targets) => {
                for target in targets {
                    emit(sink, clock, target.down())?;
                }
                sleep(clock, bind.tap);
                for target in targets.iter().rev() {
                    emit(sink, clock, target.up())?;
                }
            }
            Step::Hold(targets) => {
                for target in targets {
                    emit(sink, clock, target.down())?;
                    held.push(*target);
                }
            }
            Step::Release(targets) => {
                for target in targets {
                    emit(sink, clock, target.up())?;
                    if let Some(position) = held.iter().rposition(|h| h == target) {
                        held.remove(position);
                    }
                }
            }
            Step::Wait(_) => unreachable!("handled above"),
        }
    }
    sink.flush().map_err(Error::from)
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

/// Sleeps `duration` on the injected clock — a virtual clock makes this instant.
fn sleep(clock: &dyn Clock, duration: Duration) {
    if duration.is_zero() {
        return;
    }
    let nanos = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
    clock.sleep_until(clock.now().saturating_add_nanos(nanos));
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
        run(&profile, &mut sink, &clock, &mut pump).expect("run should succeed");
        watcher.recorded().iter().map(|e| e.action).collect()
    }

    /// A mirror is edge-for-edge: down on down, up on up, nothing in between.
    #[test]
    fn a_mirror_follows_the_triggers_edges() {
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

    /// Key auto-repeat arrives as extra downs; the target must not double-press.
    #[test]
    fn repeat_downs_do_not_double_press_a_mirror() {
        let actions = run_events(
            "[binds.PgUp]\nbind = \"volume-up\"",
            vec![press(Key::PageUp), press(Key::PageUp), release(Key::PageUp)],
        );
        assert_eq!(actions.len(), 2, "one down, one up: {actions:?}");
    }

    /// The combo from the template: chord order down, reverse order up, holds bracket.
    #[test]
    fn a_sequence_fires_in_order_with_reverse_chord_release() {
        let actions = run_events(
            "[binds.F6]\nseq = [\"HOLD ctrl\", \"space d\", \"RELEASE ctrl\"]",
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

    /// The run may end while a mirror is still held; the way out releases it.
    #[test]
    fn an_interrupted_run_releases_what_it_held() {
        let actions = run_events(
            "[binds.PgUp]\nbind = \"volume-up\"",
            vec![press(Key::PageUp)], // never released; the pump just ends
        );
        assert_eq!(
            actions,
            vec![
                EmitAction::KeyDown(Key::VolumeUp),
                EmitAction::KeyUp(Key::VolumeUp)
            ],
            "the exit drain must release the mirror"
        );
    }

    /// An unbound key does nothing — the loop must not panic or emit.
    #[test]
    fn unbound_keys_are_ignored() {
        let actions = run_events("[binds.F6]\nseq = [\"a\"]", vec![press(Key::Q)]);
        assert!(actions.is_empty(), "{actions:?}");
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
        run(&profile, &mut sink, &clock, &mut pump).expect("run should succeed");

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
