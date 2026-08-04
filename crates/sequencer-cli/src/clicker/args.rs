//! The clicker's own command-line surface.
//!
//! Split from [`crate::args`] because these are the clicker *product's* options, not the
//! program's: a scripted-sequence command will bring its own `*Args` beside this one, while
//! the shared pieces ([`GlobalArgs`](crate::args::GlobalArgs), the key parser) stay where
//! both can reach them.


use clap::{Args, ValueEnum};
use sequencer_core::clicker::{ActivationMode, ClickAction, ClickConfig};
use sequencer_core::input::Key;
use sequencer_core::time::{Duration, Period};

use crate::args::{GlobalArgs, parse_key};

/// Which mouse button to click.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum MouseButton {
    /// Primary button.
    #[default]
    Left,
    /// Scroll-wheel button.
    Middle,
    /// Secondary button.
    Right,
    /// Thumb button, conventionally "back".
    Back,
    /// Thumb button, conventionally "forward".
    Forward,
}

impl From<MouseButton> for sequencer_core::input::Button {
    fn from(value: MouseButton) -> Self {
        match value {
            MouseButton::Left => Self::Left,
            MouseButton::Middle => Self::Middle,
            MouseButton::Right => Self::Right,
            MouseButton::Back => Self::Back,
            MouseButton::Forward => Self::Forward,
        }
    }
}

/// `sequencer clicker`.
///
/// `PartialEq` but not `Eq`: `cps` is a float.
#[derive(Args, Debug, Clone, PartialEq)]
pub struct ClickerArgs {
    /// Shared options.
    #[command(flatten)]
    pub global: GlobalArgs,

    /// How many clicks or key presses per second.
    #[arg(long, default_value_t = 20.0, value_name = "RATE", value_parser = parse_cps)]
    pub cps: f64,

    /// Toggle mode: tap the activation key to start, tap again to stop.
    ///
    /// The default is hold mode, which only repeats while the key is held.
    #[arg(long)]
    pub toggle: bool,

    /// Repeat a keyboard key instead of a mouse click, for example `f`.
    #[arg(
        long = "kb-key",
        alias = "key",
        alias = "key_press",
        alias = "key-press",
        value_name = "KEY",
        value_parser = parse_key
    )]
    pub kb_key: Option<Key>,

    /// Which mouse button to click, when `--kb-key` is not given.
    #[arg(long = "m-key", alias = "button", value_enum, default_value_t = MouseButton::Left, value_name = "BUTTON")]
    pub m_key: MouseButton,

    /// The key that starts and stops the burst.
    #[arg(long, default_value = "f9", value_name = "KEY", value_parser = parse_key)]
    pub activate: Key,

    /// The key that quits.
    #[arg(long, default_value = "f8", value_name = "KEY", value_parser = parse_key)]
    pub quit: Key,

    /// Stop after this many repetitions. Zero means unlimited.
    #[arg(long, default_value_t = 0, value_name = "N")]
    pub limit: u64,

    /// How long `--key` stays down, in milliseconds.
    #[arg(long, default_value_t = 1, value_name = "MS")]
    pub key_hold_ms: u64,
    /// How long a mouse button stays down, in milliseconds. A click of zero duration is not
    /// a click as far as most applications are concerned; shortened automatically if the
    /// requested rate leaves no room for it
    #[arg(long, default_value_t = 8, value_name = "MS")]
    pub button_hold_ms: u64,

}

impl ClickerArgs {
    /// Every flag at its command-line default.
    ///
    /// Override with struct-update syntax:
    /// `ClickerArgs { cps: 30.0, ..ClickerArgs::new() }`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            global: GlobalArgs::new(),
            cps: 20.0,
            toggle: false,
            kb_key: None,
            m_key: MouseButton::Left,
            activate: Key::F9,
            quit: Key::F8,
            limit: 0,
            key_hold_ms: 1,
            button_hold_ms: 8,
        }
    }

    /// The clap-free configuration this invocation means.
    ///
    /// Infallible: every value was already checked by its parser, so a caller who built
    /// this from the command line cannot reach an error here. A caller who built it by
    /// hand can still produce a bad rate, which [`ClickConfig::to_profile`] catches.
    #[must_use]
    pub fn config(&self) -> ClickConfig {
        ClickConfig {
            cps: self.cps,
            mode: if self.toggle {
                ActivationMode::Toggle
            } else {
                ActivationMode::Hold
            },
            action: match self.kb_key {
                Some(key) => ClickAction::Key(key),
                None => ClickAction::Button(self.m_key.into()),
            },
            activate: self.activate,
            quit: self.quit,
            limit: self.limit,
            key_hold: Duration::from_millis(self.key_hold_ms),
            button_hold: Duration::from_millis(self.button_hold_ms),
        }
    }
}

impl Default for ClickerArgs {
    fn default() -> Self {
        Self::new()
    }
}

impl From<&ClickerArgs> for ClickConfig {
    fn from(args: &ClickerArgs) -> Self {
        args.config()
    }
}


pub(crate) fn parse_cps(raw: &str) -> Result<f64, String> {
    let value: f64 = raw
        .parse()
        .map_err(|_| format!("`{raw}` is not a number"))?;
    Period::from_cps(value).map_err(|err| err.to_string())?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;
    use sequencer_core::clicker::ActivationMode;
    use sequencer_core::input::Key;

    use super::*;
    use crate::args::{Cli, Command};

    /// Parses a full command line, panicking on anything the parser rejects — these cases are
    /// all supposed to be accepted, so a rejection is the test failing, not a case to handle.
    fn parse(argv: &[&str]) -> Command {
        Cli::try_parse_from(argv).expect("should parse").command
    }

    #[test]
    fn the_clicker_subcommand_parses() {
        let Command::Clicker(args) = parse(&["sequencer", "clicker", "--cps", "30", "--toggle"])
        else {
            panic!("expected clicker");
        };
        assert!((args.cps - 30.0).abs() < f64::EPSILON);
        assert!(args.toggle);
    }
    #[test]
    fn the_prototypes_key_press_spelling_still_works() {
        // The Python prototype used `--key_press`; breaking that for no reason would be
        // rude to the one user who has it in their shell history.
        for spelling in ["--key", "--key_press", "--key-press"] {
            let Command::Clicker(args) = parse(&["sequencer", "clicker", spelling, "f"]) else {
                panic!("expected clicker");
            };
            assert_eq!(args.kb_key, Some(Key::F), "{spelling}");
        }
    }
    #[test]
    fn defaults_match_the_prototype() {
        let Command::Clicker(args) = parse(&["sequencer", "clicker"]) else {
            panic!("expected clicker");
        };
        let config = args.config();
        assert!((config.cps - 20.0).abs() < f64::EPSILON);
        assert_eq!(config.mode, ActivationMode::Hold);
        assert_eq!(config.activate, Key::F9);
        assert_eq!(config.quit, Key::F8);
    }
    #[test]
    fn an_impossible_rate_is_a_usage_error_not_a_runtime_surprise() {
        for bad in ["0", "-5", "nan", "inf"] {
            let result = Cli::try_parse_from(["sequencer", "clicker", "--cps", bad]);
            assert!(result.is_err(), "--cps {bad} should have been rejected");
        }
    }
    #[test]
    fn an_unknown_key_name_is_a_usage_error() {
        let err = Cli::try_parse_from(["sequencer", "clicker", "--activate", "nosuchkey"])
            .expect_err("should reject");
        assert!(err.to_string().contains("nosuchkey"));
    }
    #[test]
    fn struct_update_syntax_works_across_the_crate_boundary() {
        // The documented idiom for embedders. If `#[non_exhaustive]` ever gets added to
        // these structs, this stops compiling -- which is the point of the test.
        let args = ClickerArgs {
            cps: 12.5,
            toggle: true,
            ..ClickerArgs::new()
        };
        assert!((args.config().cps - 12.5).abs() < f64::EPSILON);
        assert_eq!(args.config().mode, ActivationMode::Toggle);
    }
}
