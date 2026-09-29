//! What a clicker *means*: the settings a user gives ([`ClickConfig`]), what to repeat
//! ([`ClickAction`]), and how the trigger behaves ([`ActivationMode`]). Lowering them into
//! a binds profile is the parent module's job; nothing here knows about the format.

use sequencer_core::input::{Button, Key};
use sequencer_core::time::Duration;

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

/// Everything `sequencer clicker` needs to know.
///
/// Every field is public and [`ClickConfig::new`] fills in the defaults, so
/// `ClickConfig { cps: 30.0, ..ClickConfig::new() }` survives a field being added later.
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
    /// buttons are the more suspicious of the two; the lowering shortens it when the
    /// requested rate leaves no room.
    pub button_hold: Duration,
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
            button_hold: Duration::from_millis(8),
        }
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

    #[test]
    fn the_defaults_match_the_python_prototype() {
        let config = ClickConfig::new();
        assert!((config.cps - 20.0).abs() < f64::EPSILON);
        assert_eq!(config.mode, ActivationMode::Hold);
        assert_eq!(config.action, ClickAction::Button(Button::Left));
        assert_eq!(config.activate, Key::F9);
        assert_eq!(config.quit, Key::F8);
        assert_eq!(config.limit, 0, "unlimited");
    }
}
