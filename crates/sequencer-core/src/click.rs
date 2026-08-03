//! The autoclicker, expressed as a profile.
//!
//! This is the whole of v1's configuration layer, and it is deliberately not a file
//! format. [`ClickConfig`] is plain data with no dependency on clap, and
//! [`ClickConfig::to_profile`] lowers it into the same [`Profile`] a config file will
//! eventually produce. Shipping the stable representation before shipping a syntax for it
//! means the TOML front-end, when it arrives, is purely additive — and means nothing here
//! is locked to a format decision made on day one.

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
}

impl ClickConfig {
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
