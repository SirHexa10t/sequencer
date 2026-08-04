//! The clicker: hold or toggle a key to repeat a click or a key press.
//!
//! One *product* built on this crate's general machinery, kept in its own directory because
//! more are planned — a scripted sequence runner next — and they will share everything below
//! rather than everything being reshaped around whichever came first.
//!
//! What lives here is only what a clicker means: the settings a user gives ([`ClickConfig`]),
//! what to repeat ([`ClickAction`]), and how the trigger behaves ([`ActivationMode`]) — plus
//! [`ClickConfig::to_profile`], which lowers all of that into the general
//! [`Profile`](crate::ir::Profile) the engine runs.
//!
//! What deliberately stays out: the engine, the step IR, emit/input types, timing. Those are
//! not "clicker parts" — they are the substrate a sequence runner will build on identically,
//! and a second product should need no changes down there at all.

use alloc::vec;

use crate::emit::EmitAction;
use crate::input::{Button, Key};
use crate::ir::{
    Binding, BindingId, CancelPolicy, CatchUp, Control, Edge, Epilogue, OtherKey, Profile, Program,
    ProgramId, RepeatMode, RepeatSpec, Step, Trigger, TriggerMode, WaitSpec,
};
use crate::time::{Duration, Period};
use crate::validate::ConfigError;

/// How the activation key behaves.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ActivationMode {
    /// Repeat while the key is held. The prototype's default.
    #[default]
    Hold,
    /// Tap to start, tap again to stop.
    Toggle,
}

/// What gets repeated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClickAction {
    /// Click a mouse button.
    Button(Button),
    /// Tap a keyboard key.
    Key(Key),
}

impl Default for ClickAction {
    fn default() -> Self {
        Self::Button(Button::Left)
    }
}

/// Everything `sequencer click` needs to know.
///
/// Every field is public and [`ClickConfig::new`] fills in the defaults, so an embedder
/// writes `ClickConfig { cps: 30.0, ..ClickConfig::new() }` and a field added later
/// cannot break their build.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ClickConfig {
    /// Repetitions per second.
    pub cps: f64,
    /// Hold or toggle.
    pub mode: ActivationMode,
    /// What to repeat.
    pub action: ClickAction,
    /// The key that starts and stops it.
    pub activate: Key,
    /// The key that quits.
    pub quit: Key,
    /// Stop after this many repetitions; zero means unlimited.
    pub limit: u64,
    /// How long a key stays down when [`ClickAction::Key`] is in use.
    ///
    /// The prototype slept 1 ms between press and release. Here that is an explicit step
    /// rather than an implicit sleep, because some applications drop a key event pair
    /// delivered in the same instant.
    pub key_hold: Duration,
    /// How long a mouse button stays down when [`ClickAction::Button`] is in use.
    ///
    /// The same reason as [`Self::key_hold`], and for a while this field did not exist —
    /// the button path emitted press and release back to back, so both left in one event
    /// packet bearing one timestamp. Everything below the toolkit accepted that happily:
    /// the kernel took the writes, a device read-back counted every one, and libinput
    /// reported the device as a working pointer. Applications still saw nothing, because a
    /// click of zero duration is not a click. Longer than the key default because pointer
    /// buttons are the more suspicious of the two, and [`Self::to_profile`] shortens it
    /// when the requested rate leaves no room.
    pub button_hold: Duration,
}

impl ClickConfig {
    /// A hold that fits inside `period`: at most half of it, so the button spends a while
    /// down and a while up. An 8 ms hold at 200/s would otherwise outlast the whole 5 ms
    /// slot, and the button would never come back up between clicks.
    fn hold_within(hold: Duration, period: Period) -> Duration {
        let half = Duration::from_nanos(period.nanos() / 2);
        if hold > half { half } else { hold }
    }

    /// Feature parity with the Python prototype: hold F9 for 20 left clicks a second,
    /// F8 quits.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cps: 20.0,
            mode: ActivationMode::Hold,
            action: ClickAction::Button(Button::Left),
            activate: Key::F9,
            quit: Key::F8,
            limit: 0,
            key_hold: Duration::from_millis(1),
            button_hold: Duration::from_millis(8),
        }
    }

    /// Lowers this into a profile the engine can run.
    ///
    /// # Errors
    ///
    /// [`ConfigError::CpsOutOfRange`] if the rate is not finite and positive, and
    /// [`ConfigError::ControlShadowsBinding`] if the activation and quit keys are the
    /// same.
    pub fn to_profile(&self) -> Result<Profile, ConfigError> {
        let period = Period::from_cps(self.cps)?;

        let steps = match self.action {
            ClickAction::Button(button) => vec![
                Step::Emit(EmitAction::ButtonDown(button)),
                Step::Wait(WaitSpec::fixed(Self::hold_within(self.button_hold, period))),
                Step::Emit(EmitAction::ButtonUp(button)),
            ],
            ClickAction::Key(key) => vec![
                Step::Emit(EmitAction::KeyDown(key)),
                Step::Wait(WaitSpec::fixed(self.key_hold)),
                Step::Emit(EmitAction::KeyUp(key)),
            ],
        };

        let repeat = RepeatSpec {
            mode: RepeatMode::Paced {
                period,
                catch_up: CatchUp::Skip,
            },
            max_iters: u32::try_from(self.limit).ok().filter(|limit| *limit > 0),
        };

        let mode = match self.mode {
            ActivationMode::Hold => TriggerMode::WhileHeld { repeat },
            // On release, matching the prototype. Toggling on press would flap the latch
            // once per OS auto-repeat while the key is held down.
            ActivationMode::Toggle => TriggerMode::Toggle {
                on: Edge::Release,
                repeat,
            },
        };

        Ok(Profile {
            name: "click".into(),
            programs: vec![Program {
                name: "click".into(),
                steps,
            }],
            bindings: vec![Binding {
                id: BindingId(0),
                trigger: Trigger::key(self.activate),
                mode,
                program: ProgramId(0),
                cancel: CancelPolicy {
                    // In toggle mode the latch decides; releasing must not also stop it.
                    on_trigger_release: self.mode == ActivationMode::Hold,
                    on_other_key: OtherKey::Ignore,
                    on_timeout: None,
                    epilogue: Epilogue::FinishIteration,
                },
                input_level: 0,
            }],
            controls: vec![(Trigger::key(self.quit), Control::Quit)],
        })
    }
}

impl Default for ClickConfig {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A click has to have a DURATION. Press and release emitted back to back leave in one
    /// event packet with one timestamp, and applications discard that — which is precisely
    /// how this failed: every layer below the toolkit reported success (the kernel took the
    /// writes, a device read-back counted them, libinput listed a healthy pointer) while not
    /// one click arrived anywhere.
    #[test]
    fn a_button_click_holds_before_releasing() {
        let profile = ClickConfig::new().to_profile().expect("the defaults are runnable");
        let steps = &profile.programs.first().expect("one program").steps;
        let waits = steps
            .iter()
            .filter(|step| matches!(step, Step::Wait(_)))
            .count();
        assert_eq!(waits, 1, "press and release must be separated: {steps:?}");
        assert!(
            matches!(steps.first(), Some(Step::Emit(EmitAction::ButtonDown(_)))),
            "{steps:?}"
        );
        assert!(
            matches!(steps.last(), Some(Step::Emit(EmitAction::ButtonUp(_)))),
            "{steps:?}"
        );
    }

    /// The hold must not swallow the interval it lives in: at a rate whose period is shorter
    /// than the default hold, the button would go down and never come back up between clicks.
    #[test]
    fn the_hold_shrinks_to_fit_a_fast_rate() {
        let fast = ClickConfig {
            cps: 500.0, // a 2ms period, against an 8ms default hold
            ..ClickConfig::new()
        };
        let profile = fast.to_profile().expect("runnable");
        let steps = &profile.programs.first().expect("one program").steps;
        let Some(Step::Wait(spec)) = steps.iter().find(|s| matches!(s, Step::Wait(_))) else {
            panic!("no hold at all: {steps:?}")
        };
        let period = u128::from(Period::from_cps(500.0).expect("valid").nanos());
        let hold = spec.base.as_nanos();
        assert!(hold > 0, "the hold must not vanish entirely");
        assert!(hold <= period / 2, "a {hold}ns hold does not fit a {period}ns period");
    }
    use crate::validate::CompiledProfile;

    #[test]
    fn the_defaults_match_the_python_prototype() {
        let config = ClickConfig::new();
        assert!((config.cps - 20.0).abs() < f64::EPSILON);
        assert_eq!(config.mode, ActivationMode::Hold);
        assert_eq!(config.action, ClickAction::Button(Button::Left));
        assert_eq!(config.activate, Key::F9);
        assert_eq!(config.quit, Key::F8);
    }

    #[test]
    fn the_default_profile_validates() {
        let profile = ClickConfig::new().to_profile().expect("defaults are valid");
        assert!(CompiledProfile::validate(profile).is_ok());
    }

    #[test]
    fn hold_mode_stops_on_release_and_toggle_mode_does_not() {
        let hold = ClickConfig::new().to_profile().unwrap();
        assert!(hold.bindings[0].cancel.on_trigger_release);

        let toggle = ClickConfig {
            mode: ActivationMode::Toggle,
            ..ClickConfig::new()
        }
        .to_profile()
        .unwrap();
        assert!(!toggle.bindings[0].cancel.on_trigger_release);
        assert!(matches!(
            toggle.bindings[0].mode,
            TriggerMode::Toggle {
                on: Edge::Release,
                ..
            }
        ));
    }

    #[test]
    fn key_mode_inserts_the_hold_as_an_explicit_step() {
        let profile = ClickConfig {
            action: ClickAction::Key(Key::F),
            key_hold: Duration::from_millis(3),
            ..ClickConfig::new()
        }
        .to_profile()
        .unwrap();
        assert_eq!(
            profile.programs[0].steps,
            vec![
                Step::Emit(EmitAction::KeyDown(Key::F)),
                Step::Wait(WaitSpec::fixed(Duration::from_millis(3))),
                Step::Emit(EmitAction::KeyUp(Key::F)),
            ]
        );
    }

    #[test]
    fn a_zero_limit_means_unlimited() {
        let unlimited = ClickConfig::new().to_profile().unwrap();
        let TriggerMode::WhileHeld { repeat } = unlimited.bindings[0].mode else {
            panic!("expected hold mode");
        };
        assert_eq!(repeat.max_iters, None);

        let capped = ClickConfig {
            limit: 5,
            ..ClickConfig::new()
        }
        .to_profile()
        .unwrap();
        let TriggerMode::WhileHeld { repeat } = capped.bindings[0].mode else {
            panic!("expected hold mode");
        };
        assert_eq!(repeat.max_iters, Some(5));
    }

    #[test]
    fn a_bad_rate_is_rejected_before_anything_runs() {
        for bad in [0.0, -1.0, f64::NAN] {
            let config = ClickConfig {
                cps: bad,
                ..ClickConfig::new()
            };
            assert!(matches!(
                config.to_profile(),
                Err(ConfigError::CpsOutOfRange(_))
            ));
        }
        // No upper limit: an absurd rate lowers to a valid profile, and the engine skips
        // whatever slots the machine cannot deliver.
        let config = ClickConfig {
            cps: 1e9,
            ..ClickConfig::new()
        };
        assert!(config.to_profile().is_ok());
    }

    #[test]
    fn binding_the_quit_key_to_the_action_is_caught_by_validation() {
        let profile = ClickConfig {
            quit: Key::F9,
            ..ClickConfig::new()
        }
        .to_profile()
        .expect("lowering succeeds; the conflict is a validation matter");
        assert!(matches!(
            CompiledProfile::validate(profile),
            Err(ConfigError::ControlShadowsBinding { .. })
        ));
    }
}
