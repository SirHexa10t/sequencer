//! Turning a [`Profile`] into something the engine can run without checking anything.
//!
//! The flat, index-addressed step list buys a one-integer program counter and trivially
//! cheap cancellation. Its honest cost is that indices can be wrong, so somebody has to
//! prove they aren't. That is this module, and isolating it here is what lets
//! [`crate::Engine::tick`] be infallible: by the time the engine sees a profile, every
//! jump target is in bounds and every loop is balanced.

use alloc::vec::Vec;

use crate::ir::{
    Binding, BindingId, Epilogue, LoopCount, Profile, Program, ProgramId, Step, StepIx, Trigger,
};

/// A profile that could not be run as written.
#[derive(Clone, PartialEq, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// A rate that is not a finite positive number.
    #[error("clicks per second must be a finite positive number, got {0}")]
    CpsOutOfRange(f64),
    /// A binding names a program that does not exist.
    #[error("binding {} refers to program {} which does not exist", .binding.0, .program.0)]
    NoSuchProgram {
        /// The offending binding.
        binding: BindingId,
        /// The program it named.
        program: ProgramId,
    },
    /// A step's jump target is past the end of its program.
    #[error("step {step} of program {} jumps to {target}, past the end of the program", .program.0)]
    JumpOutOfBounds {
        /// The program containing the bad step.
        program: ProgramId,
        /// The offending step.
        step: StepIx,
        /// Where it tried to go.
        target: StepIx,
    },
    /// A `LoopStart` has no matching `LoopEnd`, or vice versa.
    #[error("program {} has an unbalanced loop at step {step}", .program.0)]
    UnbalancedLoop {
        /// The program containing the bad loop.
        program: ProgramId,
        /// Where the imbalance was found.
        step: StepIx,
    },
    /// Two bindings would both fire on the same input at the same level.
    #[error("two bindings ({}, {}) claim the same trigger at input level {level}", .first.0, .second.0)]
    DuplicateTrigger {
        /// The binding declared first.
        first: BindingId,
        /// The binding that collides with it.
        second: BindingId,
        /// The input level they share.
        level: u8,
    },
    /// A runner-level control shares a trigger with a binding.
    #[error("binding {} shares its trigger with a runner control, so the trigger is ambiguous", .binding.0)]
    ControlShadowsBinding {
        /// The binding that collides with a control.
        binding: BindingId,
    },
    /// A program is empty, so the binding would do nothing.
    #[error("program {} (\"{name}\") has no steps", .program.0)]
    EmptyProgram {
        /// The empty program.
        program: ProgramId,
        /// Its name, for the message.
        name: alloc::boxed::Box<str>,
    },
    /// A cleanup section starts inside a loop body.
    #[error(
        "binding {}'s cleanup starts at step {step} of program {}, which is inside a loop; \
         cleanup must start outside every loop",
        .binding.0,
        .program.0
    )]
    CleanupInsideLoop {
        /// The binding whose epilogue is at fault.
        binding: BindingId,
        /// The program it runs.
        program: ProgramId,
        /// The offending target.
        step: StepIx,
    },
}

/// A [`Profile`] whose indices are proven good.
///
/// Constructed only by [`CompiledProfile::validate`], so holding one is evidence the
/// checks ran. Derefs to the underlying profile for reading.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CompiledProfile {
    profile: Profile,
}

impl CompiledProfile {
    /// Checks a profile and wraps it.
    ///
    /// # Errors
    ///
    /// Returns the first problem found: a dangling program reference, an out-of-bounds
    /// jump, an unbalanced loop, an empty program, or two rules claiming one trigger.
    pub fn validate(profile: Profile) -> Result<Self, ConfigError> {
        for (index, program) in profile.programs.iter().enumerate() {
            let id = ProgramId(u32::try_from(index).unwrap_or(u32::MAX));
            check_program(id, program)?;
        }
        for binding in &profile.bindings {
            check_binding(&profile, binding)?;
        }
        check_trigger_conflicts(&profile)?;
        Ok(Self { profile })
    }

    /// The profile being run.
    #[must_use]
    pub const fn profile(&self) -> &Profile {
        &self.profile
    }

    /// The program a binding runs.
    ///
    /// Infallible: validation proved the reference resolves.
    #[must_use]
    pub fn program_for(&self, binding: &Binding) -> &Program {
        &self.profile.programs[binding.program.0 as usize]
    }
}

impl core::ops::Deref for CompiledProfile {
    type Target = Profile;

    fn deref(&self) -> &Self::Target {
        &self.profile
    }
}

/// Proves every jump in one program lands inside it and every loop is balanced.
fn check_program(id: ProgramId, program: &Program) -> Result<(), ConfigError> {
    if program.steps.is_empty() {
        return Err(ConfigError::EmptyProgram {
            program: id,
            name: program.name.clone(),
        });
    }

    let len = StepIx::try_from(program.steps.len()).unwrap_or(StepIx::MAX);
    let mut open: Vec<StepIx> = Vec::new();

    for (index, step) in program.steps.iter().enumerate() {
        let here = StepIx::try_from(index).unwrap_or(StepIx::MAX);
        match *step {
            Step::LoopStart { end, .. } => {
                // `end` is the index just past the matching LoopEnd, so it may equal len.
                if end > len {
                    return Err(ConfigError::JumpOutOfBounds {
                        program: id,
                        step: here,
                        target: end,
                    });
                }
                open.push(here);
            }
            Step::LoopEnd { start } => {
                let Some(opened) = open.pop() else {
                    return Err(ConfigError::UnbalancedLoop {
                        program: id,
                        step: here,
                    });
                };
                // The pair must point at each other, or the executor's jumps would
                // silently run the wrong body.
                let matches = opened == start
                    && matches!(
                        program.steps[start as usize],
                        Step::LoopStart { end, .. } if end == here + 1
                    );
                if !matches {
                    return Err(ConfigError::UnbalancedLoop {
                        program: id,
                        step: here,
                    });
                }
            }
            Step::Emit(_) | Step::Wait(_) => {}
        }
    }

    match open.first() {
        Some(&unclosed) => Err(ConfigError::UnbalancedLoop {
            program: id,
            step: unclosed,
        }),
        None => Ok(()),
    }
}

/// Proves a binding's program exists and its cleanup jump lands inside that program.
fn check_binding(profile: &Profile, binding: &Binding) -> Result<(), ConfigError> {
    let Some(program) = profile.programs.get(binding.program.0 as usize) else {
        return Err(ConfigError::NoSuchProgram {
            binding: binding.id,
            program: binding.program,
        });
    };

    if let Epilogue::RunTail { from } = binding.cancel.epilogue {
        let len = StepIx::try_from(program.steps.len()).unwrap_or(StepIx::MAX);
        if from >= len {
            return Err(ConfigError::JumpOutOfBounds {
                program: binding.program,
                step: from,
                target: from,
            });
        }
        // Jumping into a loop body would leave the executor with an empty loop stack when
        // it reached the matching `LoopEnd`, and there is no sensible way to invent the
        // missing frame. Rejecting it here keeps `Engine::tick` infallible.
        if !top_level_steps(&program.steps).contains(&from) {
            return Err(ConfigError::CleanupInsideLoop {
                binding: binding.id,
                program: binding.program,
                step: from,
            });
        }
    }
    Ok(())
}

/// The steps that sit outside every loop body, and so are safe to jump to.
///
/// Exposed because config front-ends want it too: "step 7 is inside a loop, try 3 or 9"
/// is a far better diagnostic than "invalid target", and this is where the answer lives.
#[must_use]
pub fn top_level_steps(steps: &[Step]) -> Vec<StepIx> {
    let mut found = Vec::new();
    let mut depth = 0_u32;
    for (index, step) in steps.iter().enumerate() {
        // Depth *before* the step, so a `LoopEnd` closing the outermost loop sits at
        // depth 1 and is correctly excluded.
        if depth == 0 {
            found.push(StepIx::try_from(index).unwrap_or(StepIx::MAX));
        }
        match step {
            Step::LoopStart { .. } => depth += 1,
            Step::LoopEnd { .. } => depth = depth.saturating_sub(1),
            Step::Emit(_) | Step::Wait(_) => {}
        }
    }
    found
}

/// Proves no two rules claim the same input.
///
/// A quadratic scan, which is the right call: profiles have tens of bindings, this runs
/// once at load, and the alternative needs `Trigger` to be hashable forever.
fn check_trigger_conflicts(profile: &Profile) -> Result<(), ConfigError> {
    for (index, binding) in profile.bindings.iter().enumerate() {
        for other in &profile.bindings[index + 1..] {
            if same_trigger(binding.trigger, other.trigger)
                && binding.input_level == other.input_level
            {
                return Err(ConfigError::DuplicateTrigger {
                    first: binding.id,
                    second: other.id,
                    level: binding.input_level,
                });
            }
        }
        if profile
            .controls
            .iter()
            .any(|(trigger, _)| same_trigger(*trigger, binding.trigger))
        {
            return Err(ConfigError::ControlShadowsBinding {
                binding: binding.id,
            });
        }
    }
    Ok(())
}

fn same_trigger(a: Trigger, b: Trigger) -> bool {
    a.input == b.input && a.mods == b.mods
}

/// Whether a loop body can spin without ever yielding.
///
/// Not an error — the engine's per-tick step budget keeps such a program from wedging the
/// runner — but it is almost always a mistake, so the runner logs it at load.
#[must_use]
pub fn unbounded_spin_loops(profile: &Profile) -> Vec<(ProgramId, StepIx)> {
    let mut found = Vec::new();
    for (index, program) in profile.programs.iter().enumerate() {
        let id = ProgramId(u32::try_from(index).unwrap_or(u32::MAX));
        for (at, step) in program.steps.iter().enumerate() {
            let Step::LoopStart {
                count: LoopCount::Forever,
                end,
            } = *step
            else {
                continue;
            };
            let start = at + 1;
            let body = &program.steps
                [start.min(program.steps.len())..(end as usize).min(program.steps.len())];
            if !body.iter().any(|s| matches!(s, Step::Wait(_))) {
                found.push((id, StepIx::try_from(at).unwrap_or(StepIx::MAX)));
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emit::EmitAction;
    use crate::input::{Button, Key};
    use crate::ir::{CancelPolicy, Control, RepeatSpec, Trigger, TriggerMode, WaitSpec};
    use crate::time::{Duration, Period};
    use alloc::vec;

    fn click_steps() -> Vec<Step> {
        vec![
            Step::Emit(EmitAction::ButtonDown(Button::Left)),
            Step::Emit(EmitAction::ButtonUp(Button::Left)),
        ]
    }

    fn binding(id: u32, trigger: Trigger, program: u32) -> Binding {
        Binding {
            id: BindingId(id),
            trigger,
            mode: TriggerMode::WhileHeld {
                repeat: RepeatSpec::paced(Period::from_cps(20.0).unwrap()),
            },
            program: ProgramId(program),
            cancel: CancelPolicy::default(),
            input_level: 0,
        }
    }

    fn profile_with(programs: Vec<Program>, bindings: Vec<Binding>) -> Profile {
        Profile {
            name: "test".into(),
            programs,
            bindings,
            controls: Vec::new(),
        }
    }

    fn one_program(steps: Vec<Step>) -> Profile {
        profile_with(
            vec![Program {
                name: "p".into(),
                steps,
            }],
            vec![binding(0, Trigger::key(Key::F9), 0)],
        )
    }

    #[test]
    fn a_plain_click_profile_validates() {
        assert!(CompiledProfile::validate(one_program(click_steps())).is_ok());
    }

    #[test]
    fn balanced_loops_validate() {
        // LoopStart at 0, body at 1, LoopEnd at 2, so `end` is 3.
        let steps = vec![
            Step::LoopStart {
                count: LoopCount::Times(3),
                end: 3,
            },
            Step::Emit(EmitAction::ButtonDown(Button::Left)),
            Step::LoopEnd { start: 0 },
        ];
        assert!(CompiledProfile::validate(one_program(steps)).is_ok());
    }

    #[test]
    fn a_loop_end_with_no_start_is_rejected() {
        let steps = vec![Step::LoopEnd { start: 0 }];
        assert!(matches!(
            CompiledProfile::validate(one_program(steps)),
            Err(ConfigError::UnbalancedLoop { .. })
        ));
    }

    #[test]
    fn a_loop_start_with_no_end_is_rejected() {
        let steps = vec![
            Step::LoopStart {
                count: LoopCount::Forever,
                end: 2,
            },
            Step::Emit(EmitAction::ButtonDown(Button::Left)),
        ];
        assert!(matches!(
            CompiledProfile::validate(one_program(steps)),
            Err(ConfigError::UnbalancedLoop { .. })
        ));
    }

    #[test]
    fn a_loop_pair_that_does_not_point_at_each_other_is_rejected() {
        let steps = vec![
            Step::LoopStart {
                count: LoopCount::Times(2),
                end: 2,
            }, // should be 3
            Step::Emit(EmitAction::ButtonDown(Button::Left)),
            Step::LoopEnd { start: 0 },
        ];
        assert!(matches!(
            CompiledProfile::validate(one_program(steps)),
            Err(ConfigError::UnbalancedLoop { .. })
        ));
    }

    #[test]
    fn a_jump_past_the_end_is_rejected() {
        let steps = vec![
            Step::LoopStart {
                count: LoopCount::Times(1),
                end: 99,
            },
            Step::LoopEnd { start: 0 },
        ];
        assert!(matches!(
            CompiledProfile::validate(one_program(steps)),
            Err(ConfigError::JumpOutOfBounds { .. })
        ));
    }

    #[test]
    fn a_dangling_program_reference_is_rejected() {
        let p = profile_with(
            vec![Program {
                name: "p".into(),
                steps: click_steps(),
            }],
            vec![binding(0, Trigger::key(Key::F9), 7)],
        );
        assert!(matches!(
            CompiledProfile::validate(p),
            Err(ConfigError::NoSuchProgram { .. })
        ));
    }

    #[test]
    fn an_empty_program_is_rejected() {
        assert!(matches!(
            CompiledProfile::validate(one_program(Vec::new())),
            Err(ConfigError::EmptyProgram { .. })
        ));
    }

    #[test]
    fn two_bindings_on_one_trigger_are_rejected() {
        let p = profile_with(
            vec![Program {
                name: "p".into(),
                steps: click_steps(),
            }],
            vec![
                binding(0, Trigger::key(Key::F9), 0),
                binding(1, Trigger::key(Key::F9), 0),
            ],
        );
        assert!(matches!(
            CompiledProfile::validate(p),
            Err(ConfigError::DuplicateTrigger { .. })
        ));
    }

    #[test]
    fn the_same_trigger_at_different_levels_is_allowed() {
        let mut second = binding(1, Trigger::key(Key::F9), 0);
        second.input_level = 1;
        let p = profile_with(
            vec![Program {
                name: "p".into(),
                steps: click_steps(),
            }],
            vec![binding(0, Trigger::key(Key::F9), 0), second],
        );
        assert!(CompiledProfile::validate(p).is_ok());
    }

    #[test]
    fn different_inputs_do_not_collide() {
        let p = profile_with(
            vec![Program {
                name: "p".into(),
                steps: click_steps(),
            }],
            vec![
                binding(0, Trigger::key(Key::F9), 0),
                binding(1, Trigger::button(Button::Back), 0),
            ],
        );
        assert!(CompiledProfile::validate(p).is_ok());
    }

    #[test]
    fn a_binding_shadowed_by_a_control_is_rejected() {
        let mut p = profile_with(
            vec![Program {
                name: "p".into(),
                steps: click_steps(),
            }],
            vec![binding(0, Trigger::key(Key::F8), 0)],
        );
        p.controls.push((Trigger::key(Key::F8), Control::Quit));
        assert!(matches!(
            CompiledProfile::validate(p),
            Err(ConfigError::ControlShadowsBinding { .. })
        ));
    }

    #[test]
    fn a_cleanup_jump_past_the_end_is_rejected() {
        let mut b = binding(0, Trigger::key(Key::F9), 0);
        b.cancel.epilogue = Epilogue::RunTail { from: 9 };
        let p = profile_with(
            vec![Program {
                name: "p".into(),
                steps: click_steps(),
            }],
            vec![b],
        );
        assert!(matches!(
            CompiledProfile::validate(p),
            Err(ConfigError::JumpOutOfBounds { .. })
        ));
    }

    #[test]
    fn a_cleanup_jump_into_a_loop_body_is_rejected() {
        let steps = vec![
            Step::Emit(EmitAction::ButtonDown(Button::Left)),
            Step::LoopStart {
                count: LoopCount::Times(2),
                end: 4,
            },
            Step::Emit(EmitAction::ButtonUp(Button::Left)),
            Step::LoopEnd { start: 1 },
            Step::Emit(EmitAction::ButtonUp(Button::Left)),
        ];
        assert_eq!(top_level_steps(&steps), vec![0, 1, 4]);

        let mut inside = binding(0, Trigger::key(Key::F9), 0);
        inside.cancel.epilogue = Epilogue::RunTail { from: 2 };
        let p = profile_with(
            vec![Program {
                name: "p".into(),
                steps: steps.clone(),
            }],
            vec![inside],
        );
        assert!(matches!(
            CompiledProfile::validate(p),
            Err(ConfigError::CleanupInsideLoop { .. })
        ));

        let mut outside = binding(0, Trigger::key(Key::F9), 0);
        outside.cancel.epilogue = Epilogue::RunTail { from: 4 };
        let p = profile_with(
            vec![Program {
                name: "p".into(),
                steps,
            }],
            vec![outside],
        );
        assert!(CompiledProfile::validate(p).is_ok());
    }

    #[test]
    fn spin_loops_are_reported_but_not_fatal() {
        let spinning = vec![
            Step::LoopStart {
                count: LoopCount::Forever,
                end: 3,
            },
            Step::Emit(EmitAction::ButtonDown(Button::Left)),
            Step::LoopEnd { start: 0 },
        ];
        let profile = one_program(spinning);
        assert_eq!(unbounded_spin_loops(&profile), vec![(ProgramId(0), 0)]);
        assert!(CompiledProfile::validate(profile).is_ok());

        let paced = vec![
            Step::LoopStart {
                count: LoopCount::Forever,
                end: 3,
            },
            Step::Wait(WaitSpec::fixed(Duration::from_millis(1))),
            Step::LoopEnd { start: 0 },
        ];
        assert!(unbounded_spin_loops(&one_program(paced)).is_empty());
    }
}
