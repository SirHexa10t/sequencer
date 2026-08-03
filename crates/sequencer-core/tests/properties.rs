//! Property tests.
//!
//! Two invariants earn their keep here. **Nothing is ever left held** is the one that
//! matters: a stuck modifier key is the failure mode every comparable tool has shipped at
//! least once, and it is exactly the kind of bug that hides in the interaction between
//! cancellation, loops and shutdown rather than in any one of them. **A validated profile
//! cannot fault** is what licenses `Engine::tick` being infallible.

// Test code: unwrapping is how a test reports a failure.
#![allow(clippy::unwrap_used)]

use proptest::prelude::*;

use sequencer_core::emit::{EmitAction, EmitBuf};
use sequencer_core::input::{Button, EventKind, InputEvent, Key};
use sequencer_core::ir::{
    Binding, BindingId, CancelPolicy, Control, Edge, Epilogue, LoopCount, OtherKey, Profile,
    Program, ProgramId, RepeatMode, RepeatSpec, Step, StepIx, Trigger, TriggerInput, TriggerMode,
    WaitSpec,
};
use sequencer_core::testutil::Harness;
use sequencer_core::time::{Duration, Period, Timestamp};
use sequencer_core::validate::top_level_steps;
use sequencer_core::{CompiledProfile, Engine};

const ACTIVATE: Key = Key::F9;
const OTHER: Key = Key::Q;

/// A program before indices are assigned.
///
/// Generating a tree and flattening it is what makes every generated program balanced by
/// construction — the alternative, generating a flat list and hoping the loop indices
/// line up, would spend all its time rejecting garbage.
#[derive(Clone, Debug)]
enum Node {
    Emit(EmitAction),
    Wait(u64),
    Repeat(u32, Vec<Node>),
}

fn any_action() -> impl Strategy<Value = EmitAction> {
    prop_oneof![
        Just(EmitAction::KeyDown(Key::A)),
        Just(EmitAction::KeyUp(Key::A)),
        Just(EmitAction::KeyDown(Key::LeftShift)),
        Just(EmitAction::KeyUp(Key::LeftShift)),
        Just(EmitAction::ButtonDown(Button::Left)),
        Just(EmitAction::ButtonUp(Button::Left)),
        Just(EmitAction::ButtonDown(Button::Right)),
        Just(EmitAction::Scroll { dx: 0, dy: 1 }),
        Just(EmitAction::CursorBy { dx: 3, dy: -2 }),
    ]
}

fn any_node() -> impl Strategy<Value = Node> {
    let leaf = prop_oneof![
        3 => any_action().prop_map(Node::Emit),
        1 => (1_u64..20).prop_map(Node::Wait),
    ];
    // Only counted loops. A `Forever` loop with no `Wait` never yields, so time could not
    // advance and the run would be about the harness rather than about the engine.
    leaf.prop_recursive(3, 24, 4, |inner| {
        (0_u32..4, prop::collection::vec(inner, 1..4)).prop_map(|(n, body)| Node::Repeat(n, body))
    })
}

/// Assigns indices, producing loop pairs that point at each other.
fn flatten(nodes: &[Node], out: &mut Vec<Step>) {
    for node in nodes {
        match node {
            Node::Emit(action) => out.push(Step::Emit(*action)),
            Node::Wait(ms) => out.push(Step::Wait(WaitSpec::fixed(Duration::from_millis(*ms)))),
            Node::Repeat(count, body) => {
                let start = StepIx::try_from(out.len()).expect("programs stay small");
                out.push(Step::LoopStart {
                    count: LoopCount::Times(*count),
                    end: 0,
                });
                flatten(body, out);
                out.push(Step::LoopEnd { start });
                let end = StepIx::try_from(out.len()).expect("programs stay small");
                if let Some(Step::LoopStart { end: slot, .. }) = out.get_mut(start as usize) {
                    *slot = end;
                }
            }
        }
    }
}

fn any_program() -> impl Strategy<Value = Vec<Step>> {
    prop::collection::vec(any_node(), 1..6).prop_map(|nodes| {
        let mut steps = Vec::new();
        flatten(&nodes, &mut steps);
        // The generator can produce a tree that flattens to nothing only if every node
        // was a zero-count loop with an empty body, which the size bounds forbid; this
        // keeps the invariant explicit anyway, since validation rejects empty programs.
        if steps.is_empty() {
            steps.push(Step::Emit(EmitAction::Scroll { dx: 0, dy: 1 }));
        }
        steps
    })
}

/// Cleanup targets are drawn from the steps outside every loop, since jumping into a loop
/// body is a config error the validator rejects. Picking only valid targets is what makes
/// `RunTail` actually get exercised rather than skipped.
fn any_epilogue(targets: Vec<StepIx>) -> impl Strategy<Value = Epilogue> {
    prop_oneof![
        Just(Epilogue::Abort),
        Just(Epilogue::FinishIteration),
        prop::sample::select(targets).prop_map(|from| Epilogue::RunTail { from }),
    ]
}

fn any_cancel(targets: Vec<StepIx>) -> impl Strategy<Value = CancelPolicy> {
    (
        any::<bool>(),
        prop_oneof![
            Just(OtherKey::Ignore),
            Just(OtherKey::AnyKey),
            Just(OtherKey::Only(vec![TriggerInput::Key(OTHER)])),
        ],
        prop::option::of((1_u64..200).prop_map(Duration::from_millis)),
        any_epilogue(targets),
    )
        .prop_map(
            |(on_trigger_release, on_other_key, on_timeout, epilogue)| CancelPolicy {
                on_trigger_release,
                on_other_key,
                on_timeout,
                epilogue,
            },
        )
}

fn any_mode() -> impl Strategy<Value = TriggerMode> {
    let repeat = prop_oneof![
        Just(RepeatMode::Once),
        (5_u64..60).prop_map(|ms| RepeatMode::AfterGap {
            gap: WaitSpec::fixed(Duration::from_millis(ms))
        }),
        (5.0_f64..100.0).prop_map(|cps| RepeatMode::Paced {
            period: Period::from_cps(cps).expect("rate within range"),
            catch_up: sequencer_core::ir::CatchUp::Skip,
        }),
    ];
    let spec = (repeat, prop::option::of(1_u32..8))
        .prop_map(|(mode, max_iters)| RepeatSpec { mode, max_iters });

    prop_oneof![
        Just(TriggerMode::Once { on: Edge::Press }),
        Just(TriggerMode::Once { on: Edge::Release }),
        spec.clone()
            .prop_map(|repeat| TriggerMode::WhileHeld { repeat }),
        spec.prop_map(|repeat| TriggerMode::Toggle {
            on: Edge::Release,
            repeat
        }),
    ]
}

/// A whole scenario: what to run, how it stops, and what the user does.
#[derive(Clone, Debug)]
struct Scenario {
    steps: Vec<Step>,
    mode: TriggerMode,
    cancel: CancelPolicy,
    /// `(millisecond, is_press, is_the_trigger_key)`.
    events: Vec<(u64, bool, bool)>,
    /// Where the clock lurches, to imitate a machine under load or an NTP step.
    jumps: Vec<(u64, u64)>,
    seed: u64,
}

fn any_scenario() -> impl Strategy<Value = Scenario> {
    any_program().prop_flat_map(|steps| {
        let targets = top_level_steps(&steps);
        (
            Just(steps),
            any_mode(),
            any_cancel(targets),
            prop::collection::vec((0_u64..400, any::<bool>(), any::<bool>()), 0..8),
            prop::collection::vec((0_u64..400, 0_u64..400), 0..3),
            any::<u64>(),
        )
            .prop_map(|(steps, mode, cancel, events, jumps, seed)| Scenario {
                steps,
                mode,
                cancel,
                events,
                jumps,
                seed,
            })
    })
}

fn build(scenario: &Scenario) -> Result<CompiledProfile, sequencer_core::ConfigError> {
    CompiledProfile::validate(Profile {
        name: "property".into(),
        programs: vec![Program {
            name: "p".into(),
            steps: scenario.steps.clone(),
        }],
        bindings: vec![Binding {
            id: BindingId(0),
            trigger: Trigger::key(ACTIVATE),
            mode: scenario.mode,
            program: ProgramId(0),
            cancel: scenario.cancel.clone(),
            input_level: 0,
        }],
        controls: vec![(Trigger::key(Key::F8), Control::Quit)],
    })
}

proptest! {
    // No failure-persistence file: `tests/` has no lib.rs for proptest to sit beside, and
    // a regression corpus checked into the repo is not worth the noise here.
    #![proptest_config(ProptestConfig {
        cases: 400,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// P7: anything the flattener produces is accepted, so a well-formed program never
    /// has to be rejected at load for reasons the user cannot see.
    #[test]
    fn any_balanced_program_validates(steps in any_program()) {
        let profile = Profile {
            name: "p".into(),
            programs: vec![Program { name: "p".into(), steps }],
            bindings: Vec::new(),
            controls: Vec::new(),
        };
        prop_assert!(CompiledProfile::validate(profile).is_ok());
    }

    /// P1: whatever happens, the engine lets go of everything it took hold of.
    #[test]
    fn nothing_is_ever_left_held(scenario in any_scenario()) {
        let Ok(compiled) = build(&scenario) else {
            return Ok(()); // A generated policy the validator rejects is not interesting.
        };
        let mut harness = Harness::new(compiled, scenario.seed);

        for &(ms, press, is_trigger) in &scenario.events {
            let key = if is_trigger { ACTIVATE } else { OTHER };
            let kind = if press {
                EventKind::KeyDown(key)
            } else {
                EventKind::KeyUp(key)
            };
            harness.at_ms(ms, kind);
        }

        // Run in slices, lurching the clock between them.
        harness.run_until_ms(100);
        for &(to, extra) in &scenario.jumps {
            harness.clock().set(Timestamp::from_millis(to));
            harness.run_until_ms(to + extra);
        }
        harness.run_until_ms(600);
        harness.shutdown();

        prop_assert!(
            harness.engine().is_quiescent(),
            "still holding; timeline: {}",
            harness.timeline()
        );
        prop_assert!(
            harness.sink().has_no_leaks(),
            "pressed and never released: {:?}; timeline: {}",
            harness.sink().leaked(),
            harness.timeline()
        );
    }

    /// Cancelling promptly means never emitting after the moment of cancellation.
    #[test]
    fn shutdown_is_the_last_word(scenario in any_scenario()) {
        let Ok(compiled) = build(&scenario) else {
            return Ok(());
        };
        let mut engine = Engine::new(compiled, scenario.seed);
        let mut out = EmitBuf::new();

        let _ = engine.handle_input(InputEvent::physical(
            Timestamp::ZERO,
            EventKind::KeyDown(ACTIVATE),
        ));
        for ms in 0..200 {
            let _ = engine.tick(Timestamp::from_millis(ms), &mut out);
        }

        engine.shutdown(Timestamp::from_millis(200), &mut out);
        prop_assert!(engine.is_quiescent());

        let before_len = out.len();
        let _ = engine.tick(Timestamp::from_millis(500), &mut out);
        prop_assert_eq!(
            out.len(),
            before_len,
            "the engine emitted after shutdown"
        );
    }

    /// A rate is a ceiling: the engine must never squeeze in more iterations than the
    /// period allows, however badly the clock misbehaves.
    #[test]
    fn a_paced_repeat_never_exceeds_its_rate(
        cps in 1.0_f64..200.0,
        run_ms in 100_u64..2000,
        seed in any::<u64>(),
    ) {
        let period = Period::from_cps(cps).expect("rate within range");
        let compiled = CompiledProfile::validate(Profile {
            name: "paced".into(),
            programs: vec![Program {
                name: "click".into(),
                steps: vec![
                    Step::Emit(EmitAction::ButtonDown(Button::Left)),
                    Step::Emit(EmitAction::ButtonUp(Button::Left)),
                ],
            }],
            bindings: vec![Binding {
                id: BindingId(0),
                trigger: Trigger::key(ACTIVATE),
                mode: TriggerMode::WhileHeld { repeat: RepeatSpec::paced(period) },
                program: ProgramId(0),
                cancel: CancelPolicy::default(),
                input_level: 0,
            }],
            controls: Vec::new(),
        })
        .expect("profile should validate");

        let mut harness = Harness::new(compiled, seed);
        harness.at_ms(0, EventKind::KeyDown(ACTIVATE));
        harness.run_until_ms(run_ms);

        let clicks = harness
            .sink()
            .emitted
            .iter()
            .filter(|e| e.action == EmitAction::ButtonDown(Button::Left))
            .count() as u64;
        let allowed = run_ms * 1_000_000 / period.nanos() + 1;
        prop_assert!(
            clicks <= allowed,
            "{clicks} clicks in {run_ms}ms at {cps}/s exceeds the {allowed} the period allows"
        );
    }
}
