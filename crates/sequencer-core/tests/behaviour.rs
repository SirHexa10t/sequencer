//! Behavioural tests for the engine, against a virtual clock and a recording sink.
//!
//! The parity group pins the behaviour inherited from the original Python prototype,
//! including the parts that were accidents of its implementation — toggling on key
//! *release* rather than press, for one. The prototype itself is gone; these tests are now
//! the only record of what it did, which is the reason they spell it out.

// Test code: unwrapping is how a test reports a failure.
#![allow(clippy::unwrap_used)]

use sequencer_core::emit::{EmitAction, EmitBuf, Holdable};
use sequencer_core::input::{Button, EventKind, InputEvent, Key};
use sequencer_core::ir::{
    Binding, BindingId, CancelPolicy, Control, Edge, Epilogue, Jitter, LoopCount, OtherKey,
    Profile, Program, ProgramId, RepeatMode, RepeatSpec, Step, Trigger, TriggerInput, TriggerMode,
    WaitSpec,
};
use sequencer_core::testutil::Harness;
use sequencer_core::time::{Duration, Period, Timestamp};
use sequencer_core::{CompiledProfile, Engine};

const ACTIVATE: Key = Key::F9;
const QUIT: Key = Key::F8;

/// A press/release pair on the left button, which is what `--cps` produces.
fn click_program() -> Program {
    Program {
        name: "click".into(),
        steps: vec![
            Step::Emit(EmitAction::ButtonDown(Button::Left)),
            Step::Emit(EmitAction::ButtonUp(Button::Left)),
        ],
    }
}

/// Press, hold briefly, release: what `--kb-key f` produces.
fn key_program(key: Key, hold: Duration) -> Program {
    Program {
        name: "key".into(),
        steps: vec![
            Step::Emit(EmitAction::KeyDown(key)),
            Step::Wait(WaitSpec::fixed(hold)),
            Step::Emit(EmitAction::KeyUp(key)),
        ],
    }
}

/// One binding on F9, plus F8 wired to quit.
fn profile(program: Program, mode: TriggerMode, cancel: CancelPolicy) -> CompiledProfile {
    CompiledProfile::validate(Profile {
        name: "test".into(),
        programs: vec![program],
        bindings: vec![Binding {
            id: BindingId(0),
            trigger: Trigger::key(ACTIVATE),
            mode,
            program: ProgramId(0),
            cancel,
            input_level: 0,
        }],
        controls: vec![(Trigger::key(QUIT), Control::Quit)],
    })
    .expect("test profile should validate")
}

fn hold_at(cps: f64) -> TriggerMode {
    TriggerMode::WhileHeld {
        repeat: RepeatSpec::paced(Period::from_cps(cps).expect("valid rate")),
    }
}

fn toggle_at(cps: f64) -> TriggerMode {
    TriggerMode::Toggle {
        on: Edge::Release,
        repeat: RepeatSpec::paced(Period::from_cps(cps).expect("valid rate")),
    }
}

/// Harness for the default hold-to-click setup.
fn hold_harness(cps: f64) -> Harness {
    Harness::new(
        profile(click_program(), hold_at(cps), CancelPolicy::default()),
        0,
    )
}

fn press(ms: u64) -> (u64, EventKind) {
    (ms, EventKind::KeyDown(ACTIVATE))
}

fn release(ms: u64) -> (u64, EventKind) {
    (ms, EventKind::KeyUp(ACTIVATE))
}

/// Times, in milliseconds, at which a given action was emitted.
fn times_of(harness: &Harness, action: EmitAction) -> Vec<u64> {
    harness
        .sink()
        .emitted
        .iter()
        .filter(|emit| emit.action == action)
        .map(|emit| emit.at.nanos() / 1_000_000)
        .collect()
}

// -------------------------------------------------------- parity with the Python original

#[test]
fn hold_emits_exactly_cps_per_second() {
    let mut h = hold_harness(20.0);
    let (ms, kind) = press(0);
    h.at_ms(ms, kind);
    h.run_until_ms(999);

    let downs = times_of(&h, EmitAction::ButtonDown(Button::Left));
    assert_eq!(downs.len(), 20, "timeline: {}", h.timeline());
    assert_eq!(downs, (0..20).map(|n| n * 50).collect::<Vec<_>>());
    h.assert_quiescent();
}

#[test]
fn hold_stops_on_release() {
    let mut h = hold_harness(20.0);
    let (ms, kind) = press(0);
    h.at_ms(ms, kind);
    let (ms, kind) = release(525);
    h.at_ms(ms, kind);
    h.run_until_ms(2000);

    let downs = times_of(&h, EmitAction::ButtonDown(Button::Left));
    assert_eq!(
        downs,
        vec![0, 50, 100, 150, 200, 250, 300, 350, 400, 450, 500]
    );
    h.assert_quiescent();
}

#[test]
fn toggle_flips_on_release_not_press() {
    let mut h = Harness::new(
        profile(
            click_program(),
            toggle_at(20.0),
            CancelPolicy {
                on_trigger_release: false,
                ..CancelPolicy::default()
            },
        ),
        0,
    );
    // Press does nothing; the release at 10ms starts it. The press at 1000 does nothing
    // either; the release at 1010 stops it. This is the prototype's exact semantics.
    let (ms, kind) = press(0);
    h.at_ms(ms, kind);
    let (ms, kind) = release(10);
    h.at_ms(ms, kind);
    let (ms, kind) = press(1000);
    h.at_ms(ms, kind);
    let (ms, kind) = release(1010);
    h.at_ms(ms, kind);
    h.run_until_ms(2000);

    let downs = times_of(&h, EmitAction::ButtonDown(Button::Left));
    assert_eq!(*downs.first().expect("should have clicked"), 10);
    assert!(
        downs.iter().all(|&t| t <= 1010),
        "clicked after toggling off: {downs:?}"
    );
    // Slots fall at 10ms + 50n. The slot at exactly 1010ms does not fire: input is
    // processed before the tick for that instant, so a release wins a tie against a
    // click scheduled for the same moment. Erring towards "stop" when the user has
    // already let go is the safer of the two readings.
    assert_eq!(downs.len(), 20, "10ms..=960ms at 50ms spacing");
    h.assert_quiescent();
}

#[test]
fn key_repeat_holds_for_one_millisecond() {
    let mut h = Harness::new(
        profile(
            key_program(Key::F, Duration::from_millis(1)),
            hold_at(20.0),
            CancelPolicy::default(),
        ),
        0,
    );
    let (ms, kind) = press(0);
    h.at_ms(ms, kind);
    h.run_until_ms(120);

    assert_eq!(times_of(&h, EmitAction::KeyDown(Key::F)), vec![0, 50, 100]);
    assert_eq!(times_of(&h, EmitAction::KeyUp(Key::F)), vec![1, 51, 101]);
    h.assert_quiescent();
}

#[test]
fn an_unbound_key_is_forwarded_and_emits_nothing() {
    let mut h = hold_harness(20.0);
    h.at_ms(0, EventKind::KeyDown(Key::Q));
    h.at_ms(10, EventKind::KeyUp(Key::Q));
    h.run_until_ms(500);

    assert!(h.sink().emitted.is_empty(), "timeline: {}", h.timeline());
    assert!(!h.quit_requested());
}

// ------------------------------------------------------------------------------- timing

#[test]
fn no_drift_over_10k_iterations() {
    let mut h = hold_harness(20.0);
    let (ms, kind) = press(0);
    h.at_ms(ms, kind);
    // 10,000 iterations at 50ms is 500 virtual seconds, and costs milliseconds.
    h.run_until_ms(500_000);

    let downs = times_of(&h, EmitAction::ButtonDown(Button::Left));
    assert_eq!(downs.len(), 10_001);
    // Exact, not approximate. A `next = now + period` implementation fails on the first
    // scheduling wobble; integer accumulation cannot drift at all.
    for (n, &at) in downs.iter().enumerate() {
        assert_eq!(at, n as u64 * 50, "iteration {n} drifted");
    }
}

#[test]
fn a_fractional_period_accumulates_exactly() {
    // 3 clicks/s is 333333333ns, which does not divide evenly into a millisecond.
    let mut h = hold_harness(3.0);
    let (ms, kind) = press(0);
    h.at_ms(ms, kind);
    h.run_until_ms(10_000);

    let downs: Vec<u64> = h
        .sink()
        .emitted
        .iter()
        .filter(|e| e.action == EmitAction::ButtonDown(Button::Left))
        .map(|e| e.at.nanos())
        .collect();
    for (n, &at) in downs.iter().enumerate() {
        assert_eq!(at, n as u64 * 333_333_333, "iteration {n} drifted");
    }
}

#[test]
fn a_slow_iteration_never_overlaps_the_next() {
    // A 70ms program on a 50ms cadence cannot keep the rate. It must fall back to
    // back-to-back iterations rather than starting one before the last finished.
    let mut h = Harness::new(
        profile(
            key_program(Key::F, Duration::from_millis(70)),
            hold_at(20.0),
            CancelPolicy::default(),
        ),
        0,
    );
    let (ms, kind) = press(0);
    h.at_ms(ms, kind);
    h.run_until_ms(1000);

    let mut down = false;
    for emit in &h.sink().emitted {
        match emit.action {
            EmitAction::KeyDown(_) => {
                assert!(
                    !down,
                    "two presses with no release between: {}",
                    h.timeline()
                );
                down = true;
            }
            EmitAction::KeyUp(_) => down = false,
            _ => {}
        }
    }
    assert_eq!(
        times_of(&h, EmitAction::KeyDown(Key::F)),
        vec![
            0, 70, 140, 210, 280, 350, 420, 490, 560, 630, 700, 770, 840, 910, 980
        ]
    );
    // The run stopped 20ms into the pass that began at 980, so F is legitimately still
    // down. Releasing it is the runner's job at shutdown, which is exactly what happens.
    assert!(!h.engine().is_quiescent(), "should still be mid-press");
    h.shutdown();
    h.assert_quiescent();
}

#[test]
fn catch_up_skip_drops_missed_slots_and_keeps_phase() {
    let mut h = hold_harness(20.0);
    let (ms, kind) = press(0);
    h.at_ms(ms, kind);
    h.run_until_ms(0);
    assert_eq!(h.sink().emitted.len(), 2, "one click at t=0");

    // Simulate the process being descheduled for half a second.
    h.clock().set(Timestamp::from_millis(550));
    h.run_until_ms(550);

    let downs = times_of(&h, EmitAction::ButtonDown(Button::Left));
    assert_eq!(downs, vec![0, 550], "exactly one catch-up click, not ten");
    assert_eq!(h.stats.slots_skipped, 10);

    // Phase preserved: the next slot is still a multiple of 50ms.
    h.run_until_ms(650);
    let downs = times_of(&h, EmitAction::ButtonDown(Button::Left));
    assert_eq!(downs, vec![0, 550, 600, 650]);
}

#[test]
fn catch_up_burst_is_capped_by_its_own_type() {
    let mode = TriggerMode::WhileHeld {
        repeat: RepeatSpec {
            mode: RepeatMode::Paced {
                period: Period::from_cps(20.0).expect("valid rate"),
                catch_up: sequencer_core::ir::CatchUp::Burst { max: 3 },
            },
            max_iters: None,
        },
    };
    let mut h = Harness::new(profile(click_program(), mode, CancelPolicy::default()), 0);
    let (ms, kind) = press(0);
    h.at_ms(ms, kind);
    h.run_until_ms(0);
    h.clock().set(Timestamp::from_millis(550));
    h.run_until_ms(550);

    let at_550 = times_of(&h, EmitAction::ButtonDown(Button::Left))
        .into_iter()
        .filter(|&t| t == 550)
        .count();
    assert_eq!(at_550, 4, "the caught-up slot plus a burst of at most 3");
}

#[test]
fn a_clock_that_jumps_backwards_neither_panics_nor_storms() {
    let mut h = hold_harness(20.0);
    let (ms, kind) = press(0);
    h.at_ms(ms, kind);
    h.run_until_ms(1000);
    let before = h.sink().emitted.len();

    h.clock().set(Timestamp::from_millis(900));
    h.run_until_ms(1000);

    assert_eq!(
        h.sink().emitted.len(),
        before,
        "rewinding the clock must not replay clicks"
    );
    h.assert_quiescent();
}

#[test]
fn ticking_twice_at_the_same_instant_emits_once() {
    let mut engine = Engine::new(
        profile(click_program(), hold_at(20.0), CancelPolicy::default()),
        0,
    );
    let mut out = EmitBuf::new();
    let _ = engine.handle_input(InputEvent::physical(
        Timestamp::ZERO,
        EventKind::KeyDown(ACTIVATE),
    ));

    let _ = engine.tick(Timestamp::ZERO, &mut out);
    let after_first = out.len();
    let _ = engine.tick(Timestamp::ZERO, &mut out);
    assert_eq!(out.len(), after_first, "a repeated tick must be a no-op");

    engine.shutdown(Timestamp::ZERO, &mut out);
}

// ------------------------------------------------------------- cancellation and the ledger

#[test]
fn cancelling_mid_sequence_releases_what_is_held() {
    // Held key with a long tail, cancelled while the key is down.
    let program = Program {
        name: "long".into(),
        steps: vec![
            Step::Emit(EmitAction::KeyDown(Key::A)),
            Step::Wait(WaitSpec::fixed(Duration::from_secs(1))),
            Step::Emit(EmitAction::KeyUp(Key::A)),
        ],
    };
    let mut h = Harness::new(
        profile(
            program,
            TriggerMode::WhileHeld {
                repeat: RepeatSpec {
                    mode: RepeatMode::Once,
                    max_iters: None,
                },
            },
            CancelPolicy {
                epilogue: Epilogue::Abort,
                ..CancelPolicy::default()
            },
        ),
        0,
    );
    let (ms, kind) = press(0);
    h.at_ms(ms, kind);
    let (ms, kind) = release(100);
    h.at_ms(ms, kind);
    h.run_until_ms(2000);

    assert_eq!(times_of(&h, EmitAction::KeyDown(Key::A)), vec![0]);
    assert_eq!(
        times_of(&h, EmitAction::KeyUp(Key::A)),
        vec![100],
        "the release must land at the cancel, not at the end of the wait"
    );
    h.assert_quiescent();
}

#[test]
fn the_ledger_drains_in_reverse_order() {
    let program = Program {
        name: "chord".into(),
        steps: vec![
            Step::Emit(EmitAction::KeyDown(Key::LeftCtrl)),
            Step::Emit(EmitAction::KeyDown(Key::LeftShift)),
            Step::Emit(EmitAction::KeyDown(Key::A)),
            Step::Wait(WaitSpec::fixed(Duration::from_secs(1))),
        ],
    };
    let mut h = Harness::new(
        profile(
            program,
            TriggerMode::WhileHeld {
                repeat: RepeatSpec {
                    mode: RepeatMode::Once,
                    max_iters: None,
                },
            },
            CancelPolicy {
                epilogue: Epilogue::Abort,
                ..CancelPolicy::default()
            },
        ),
        0,
    );
    let (ms, kind) = press(0);
    h.at_ms(ms, kind);
    let (ms, kind) = release(10);
    h.at_ms(ms, kind);
    h.run_until_ms(500);

    let ups: Vec<EmitAction> = h
        .sink()
        .actions()
        .into_iter()
        .filter(|a| a.releases().is_some())
        .collect();
    assert_eq!(
        ups,
        vec![
            EmitAction::KeyUp(Key::A),
            EmitAction::KeyUp(Key::LeftShift),
            EmitAction::KeyUp(Key::LeftCtrl),
        ]
    );
}

#[test]
fn finish_iteration_completes_the_click_that_abort_would_cut_short() {
    let build = |epilogue| {
        Harness::new(
            profile(
                key_program(Key::F, Duration::from_millis(100)),
                TriggerMode::WhileHeld {
                    repeat: RepeatSpec {
                        mode: RepeatMode::Once,
                        max_iters: None,
                    },
                },
                CancelPolicy {
                    epilogue,
                    ..CancelPolicy::default()
                },
            ),
            0,
        )
    };

    // Release lands between the KeyDown and the KeyUp.
    for epilogue in [Epilogue::FinishIteration, Epilogue::Abort] {
        let mut h = build(epilogue);
        let (ms, kind) = press(0);
        h.at_ms(ms, kind);
        let (ms, kind) = release(50);
        h.at_ms(ms, kind);
        h.run_until_ms(500);

        assert_eq!(times_of(&h, EmitAction::KeyDown(Key::F)), vec![0]);
        // Either way the key comes back up, and either way nothing is left held. The
        // difference is only *when*: the scheduled step, or the cancel.
        let ups = times_of(&h, EmitAction::KeyUp(Key::F));
        match epilogue {
            Epilogue::FinishIteration => assert_eq!(ups, vec![100], "{epilogue:?}"),
            _ => assert_eq!(ups, vec![50], "{epilogue:?}"),
        }
        h.assert_quiescent();
    }
}

#[test]
fn pressing_another_key_cancels_when_the_policy_says_so() {
    let mut h = Harness::new(
        profile(
            click_program(),
            hold_at(20.0),
            CancelPolicy {
                on_other_key: OtherKey::AnyKey,
                ..CancelPolicy::default()
            },
        ),
        0,
    );
    let (ms, kind) = press(0);
    h.at_ms(ms, kind);
    h.at_ms(120, EventKind::KeyDown(Key::Q));
    h.run_until_ms(1000);

    let downs = times_of(&h, EmitAction::ButtonDown(Button::Left));
    assert_eq!(downs, vec![0, 50, 100]);
    h.assert_quiescent();
}

#[test]
fn only_the_listed_keys_cancel() {
    let mut h = Harness::new(
        profile(
            click_program(),
            hold_at(20.0),
            CancelPolicy {
                on_other_key: OtherKey::Only(vec![TriggerInput::Key(Key::Escape)]),
                ..CancelPolicy::default()
            },
        ),
        0,
    );
    let (ms, kind) = press(0);
    h.at_ms(ms, kind);
    h.at_ms(120, EventKind::KeyDown(Key::Q)); // not in the list, ignored
    h.at_ms(220, EventKind::KeyDown(Key::Escape)); // in the list, cancels
    h.run_until_ms(1000);

    let downs = times_of(&h, EmitAction::ButtonDown(Button::Left));
    assert_eq!(downs, vec![0, 50, 100, 150, 200]);
}

#[test]
fn a_timeout_cancels_and_releases() {
    let program = Program {
        name: "long".into(),
        steps: vec![
            Step::Emit(EmitAction::KeyDown(Key::A)),
            Step::Wait(WaitSpec::fixed(Duration::from_secs(1))),
            Step::Emit(EmitAction::KeyUp(Key::A)),
        ],
    };
    let mut h = Harness::new(
        profile(
            program,
            TriggerMode::WhileHeld {
                repeat: RepeatSpec {
                    mode: RepeatMode::Once,
                    max_iters: None,
                },
            },
            CancelPolicy {
                on_timeout: Some(Duration::from_millis(200)),
                epilogue: Epilogue::Abort,
                ..CancelPolicy::default()
            },
        ),
        0,
    );
    let (ms, kind) = press(0);
    h.at_ms(ms, kind);
    h.run_until_ms(2000);

    assert_eq!(times_of(&h, EmitAction::KeyUp(Key::A)), vec![200]);
    h.assert_quiescent();
}

#[test]
fn shutdown_is_idempotent() {
    let mut engine = Engine::new(
        profile(
            key_program(Key::F, Duration::from_secs(1)),
            hold_at(1.0),
            CancelPolicy::default(),
        ),
        0,
    );
    let mut out = EmitBuf::new();
    let _ = engine.handle_input(InputEvent::physical(
        Timestamp::ZERO,
        EventKind::KeyDown(ACTIVATE),
    ));
    let _ = engine.tick(Timestamp::ZERO, &mut out);
    out.clear();

    engine.shutdown(Timestamp::from_millis(10), &mut out);
    assert_eq!(
        out.as_slice().iter().map(|e| e.action).collect::<Vec<_>>(),
        vec![EmitAction::KeyUp(Key::F)]
    );

    let after_first = out.len();
    engine.shutdown(Timestamp::from_millis(20), &mut out);
    assert_eq!(
        out.len(),
        after_first,
        "the second shutdown releases nothing"
    );
    assert!(engine.is_quiescent());
}

#[test]
fn the_quit_control_fires_and_shutdown_releases() {
    let mut h = Harness::new(
        profile(
            key_program(Key::F, Duration::from_secs(1)),
            hold_at(1.0),
            CancelPolicy::default(),
        ),
        0,
    );
    let (ms, kind) = press(0);
    h.at_ms(ms, kind);
    h.at_ms(100, EventKind::KeyDown(QUIT));
    h.run_until_ms(5000);

    assert!(h.quit_requested());
    h.shutdown();
    h.assert_quiescent();
    assert!(
        h.sink().has_no_leaks(),
        "left held after quit: {:?}",
        h.sink().leaked()
    );
}

// -------------------------------------------------------------------------- edge cases

#[test]
fn a_rapid_toggle_double_tap_lands_off() {
    let mut h = Harness::new(
        profile(
            click_program(),
            toggle_at(20.0),
            CancelPolicy {
                on_trigger_release: false,
                ..CancelPolicy::default()
            },
        ),
        0,
    );
    // Down/up/down/up inside 5ms. In the prototype this raced the listener thread against
    // the main loop; here every mutation happens on one thread from a queue.
    h.at_ms(0, EventKind::KeyDown(ACTIVATE));
    h.at_ms(1, EventKind::KeyUp(ACTIVATE));
    h.at_ms(3, EventKind::KeyDown(ACTIVATE));
    h.at_ms(4, EventKind::KeyUp(ACTIVATE));
    h.run_until_ms(1000);

    let downs = times_of(&h, EmitAction::ButtonDown(Button::Left));
    assert!(
        downs.iter().all(|&t| t <= 4),
        "still clicking after toggling back off: {downs:?}"
    );
    h.assert_quiescent();
}

#[test]
fn os_auto_repeat_does_not_restart_or_shift_the_cadence() {
    let mut h = hold_harness(20.0);
    h.at_ms(0, EventKind::KeyDown(ACTIVATE));
    // A held key repeats KeyDown without an intervening KeyUp. Treating those as fresh
    // presses would reset the phase on every one.
    h.at_ms(30, EventKind::KeyDown(ACTIVATE));
    h.at_ms(60, EventKind::KeyDown(ACTIVATE));
    h.at_ms(90, EventKind::KeyDown(ACTIVATE));
    h.run_until_ms(200);

    let downs = times_of(&h, EmitAction::ButtonDown(Button::Left));
    assert_eq!(downs, vec![0, 50, 100, 150, 200]);
}

#[test]
fn a_spin_loop_respects_the_step_budget_and_still_sees_the_quit_key() {
    // A Forever loop with no Wait would spin inside tick without the budget.
    let program = Program {
        name: "spin".into(),
        steps: vec![
            Step::LoopStart {
                count: LoopCount::Forever,
                end: 3,
            },
            Step::Emit(EmitAction::Scroll { dx: 0, dy: 1 }),
            Step::LoopEnd { start: 0 },
        ],
    };
    // Driven directly rather than through the harness: a program that never yields also
    // never lets virtual time advance, so "run for 100ms" is not a thing that can happen.
    // What matters is that each individual tick returns.
    let mut engine = Engine::new(
        profile(
            program,
            TriggerMode::WhileHeld {
                repeat: RepeatSpec {
                    mode: RepeatMode::Once,
                    max_iters: None,
                },
            },
            CancelPolicy::default(),
        ),
        0,
    );
    let mut out = EmitBuf::new();
    let _ = engine.handle_input(InputEvent::physical(
        Timestamp::ZERO,
        EventKind::KeyDown(ACTIVATE),
    ));

    let outcome = engine.tick(Timestamp::ZERO, &mut out);
    assert!(
        outcome.stats.budget_exhausted,
        "the budget should have cut the tick short"
    );
    assert_eq!(
        outcome.next_deadline,
        Some(Timestamp::ZERO),
        "a budget-limited tick must ask to be re-entered at once, not slept off"
    );
    // Each pass through the body costs two steps (the emit and the loop's jump back), so
    // a 1024-step budget buys around half that many actions.
    assert!(out.len() >= 500, "expected real work, got {}", out.len());

    // The runner got control back, so input still lands.
    let quit = engine.handle_input(InputEvent::physical(
        Timestamp::from_millis(1),
        EventKind::KeyDown(QUIT),
    ));
    assert_eq!(quit, Some(Control::Quit));

    engine.shutdown(Timestamp::from_millis(1), &mut out);
    assert!(engine.is_quiescent());
}

#[test]
fn synthetic_output_at_level_zero_cannot_retrigger_its_own_binding() {
    // The binding fires on F9 and emits F9. With both levels at 0, feeding the emitted
    // event straight back must not start a second run.
    let program = Program {
        name: "echo".into(),
        steps: vec![
            Step::Emit(EmitAction::KeyDown(ACTIVATE)),
            Step::Emit(EmitAction::KeyUp(ACTIVATE)),
        ],
    };
    let mut engine = Engine::new(
        profile(
            program,
            TriggerMode::WhileHeld {
                repeat: RepeatSpec {
                    mode: RepeatMode::Once,
                    max_iters: None,
                },
            },
            CancelPolicy::default(),
        ),
        0,
    );
    let mut out = EmitBuf::new();
    let _ = engine.handle_input(InputEvent::physical(
        Timestamp::ZERO,
        EventKind::KeyDown(ACTIVATE),
    ));
    let _ = engine.tick(Timestamp::ZERO, &mut out);
    let first_round = out.len();
    assert_eq!(first_round, 2);

    // Feed our own output back in, as a real backend's capture side would see it.
    for emit in out.as_slice().to_vec() {
        let _ = engine.handle_input(InputEvent::synthetic(
            emit.at,
            emit.level,
            match emit.action {
                EmitAction::KeyDown(k) => EventKind::KeyDown(k),
                EmitAction::KeyUp(k) => EventKind::KeyUp(k),
                _ => unreachable!("this program only emits key events"),
            },
        ));
    }
    let _ = engine.tick(Timestamp::from_millis(1), &mut out);
    assert_eq!(out.len(), first_round, "the engine retriggered itself");

    engine.shutdown(Timestamp::from_millis(2), &mut out);
}

#[test]
fn a_higher_level_synthetic_event_does_trigger() {
    let mut engine = Engine::new(
        profile(
            click_program(),
            TriggerMode::WhileHeld {
                repeat: RepeatSpec {
                    mode: RepeatMode::Once,
                    max_iters: None,
                },
            },
            CancelPolicy::default(),
        ),
        0,
    );
    let mut out = EmitBuf::new();
    // Level 1 out-ranks the binding's input_level of 0, which is how a deliberate remap
    // cascade would work.
    let _ = engine.handle_input(InputEvent::synthetic(
        Timestamp::ZERO,
        1,
        EventKind::KeyDown(ACTIVATE),
    ));
    let _ = engine.tick(Timestamp::ZERO, &mut out);
    assert_eq!(out.len(), 2);

    engine.shutdown(Timestamp::from_millis(1), &mut out);
}

#[test]
fn max_iters_stops_the_repeat() {
    let mode = TriggerMode::WhileHeld {
        repeat: RepeatSpec {
            mode: RepeatMode::Paced {
                period: Period::from_cps(20.0).expect("valid rate"),
                catch_up: sequencer_core::ir::CatchUp::Skip,
            },
            max_iters: Some(5),
        },
    };
    let mut h = Harness::new(profile(click_program(), mode, CancelPolicy::default()), 0);
    h.at_ms(0, EventKind::KeyDown(ACTIVATE));
    h.run_until_ms(5000);

    assert_eq!(
        times_of(&h, EmitAction::ButtonDown(Button::Left)),
        vec![0, 50, 100, 150, 200]
    );
    h.assert_quiescent();
}

#[test]
fn after_gap_paces_from_the_end_of_the_previous_iteration() {
    let mode = TriggerMode::WhileHeld {
        repeat: RepeatSpec {
            mode: RepeatMode::AfterGap {
                gap: WaitSpec::fixed(Duration::from_millis(25)),
            },
            max_iters: None,
        },
    };
    let mut h = Harness::new(
        profile(
            key_program(Key::F, Duration::from_millis(10)),
            mode,
            CancelPolicy::default(),
        ),
        0,
    );
    h.at_ms(0, EventKind::KeyDown(ACTIVATE));
    h.run_until_ms(200);

    // Each pass takes 10ms, then a 25ms gap, so starts land 35ms apart.
    assert_eq!(
        times_of(&h, EmitAction::KeyDown(Key::F)),
        vec![0, 35, 70, 105, 140, 175]
    );
    h.assert_quiescent();
}

#[test]
fn jitter_varies_the_cadence_without_leaking_holds() {
    let program = Program {
        name: "jittery".into(),
        steps: vec![
            Step::Emit(EmitAction::KeyDown(Key::F)),
            Step::Wait(WaitSpec {
                base: Duration::from_millis(5),
                jitter: Jitter::Uniform {
                    plus_minus: Duration::from_millis(4),
                },
            }),
            Step::Emit(EmitAction::KeyUp(Key::F)),
        ],
    };
    let mode = TriggerMode::WhileHeld {
        repeat: RepeatSpec {
            mode: RepeatMode::AfterGap {
                gap: WaitSpec {
                    base: Duration::from_millis(50),
                    jitter: Jitter::Uniform {
                        plus_minus: Duration::from_millis(10),
                    },
                },
            },
            max_iters: None,
        },
    };
    let mut h = Harness::new(profile(program, mode, CancelPolicy::default()), 12345);
    h.at_ms(0, EventKind::KeyDown(ACTIVATE));
    h.run_until_ms(5000);

    let downs = times_of(&h, EmitAction::KeyDown(Key::F));
    let gaps: Vec<u64> = downs.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(gaps.len() > 20, "expected plenty of iterations");
    assert!(
        gaps.iter().any(|&g| g != gaps[0]),
        "jitter produced a perfectly uniform cadence: {gaps:?}"
    );
    h.assert_quiescent();
}

#[test]
fn holding_a_second_binding_runs_both() {
    let compiled = CompiledProfile::validate(Profile {
        name: "two".into(),
        programs: vec![
            click_program(),
            key_program(Key::G, Duration::from_millis(1)),
        ],
        bindings: vec![
            Binding {
                id: BindingId(0),
                trigger: Trigger::key(ACTIVATE),
                mode: hold_at(20.0),
                program: ProgramId(0),
                cancel: CancelPolicy::default(),
                input_level: 0,
            },
            Binding {
                id: BindingId(1),
                trigger: Trigger::button(Button::Back),
                mode: hold_at(10.0),
                program: ProgramId(1),
                cancel: CancelPolicy::default(),
                input_level: 0,
            },
        ],
        controls: vec![(Trigger::key(QUIT), Control::Quit)],
    })
    .expect("profile should validate");

    let mut h = Harness::new(compiled, 0);
    h.at_ms(0, EventKind::KeyDown(ACTIVATE));
    h.at_ms(0, EventKind::ButtonDown(Button::Back));
    h.run_until_ms(199);

    assert_eq!(times_of(&h, EmitAction::ButtonDown(Button::Left)).len(), 4);
    assert_eq!(times_of(&h, EmitAction::KeyDown(Key::G)).len(), 2);
    h.assert_quiescent();
}

#[test]
fn an_unmatched_release_in_a_program_does_not_corrupt_the_ledger() {
    let program = Program {
        name: "sloppy".into(),
        steps: vec![
            Step::Emit(EmitAction::KeyUp(Key::A)), // never pressed
            Step::Emit(EmitAction::KeyDown(Key::B)),
        ],
    };
    let mut h = Harness::new(
        profile(
            program,
            TriggerMode::WhileHeld {
                repeat: RepeatSpec {
                    mode: RepeatMode::Once,
                    max_iters: None,
                },
            },
            CancelPolicy::default(),
        ),
        0,
    );
    h.at_ms(0, EventKind::KeyDown(ACTIVATE));
    h.run_until_ms(100);

    // B was pressed and never released by the program, so the boundary drain releases it.
    assert_eq!(
        h.sink().unbalanced(),
        vec![(Holdable::Key(Key::A), -1)],
        "only the bogus release should be unmatched; B must be cleaned up"
    );
    assert!(h.engine().is_quiescent());
}
