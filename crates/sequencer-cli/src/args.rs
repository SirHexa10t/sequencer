//! The clap surface.
//!
//! Two conventions here are deliberate and worth not "tidying up" later.
//!
//! **Shared flags are flattened into each `*Args` struct rather than declared
//! `global = true` on the root.** A consumer that lifts one subcommand into its own
//! parser never parses our root, so a root-level global would be unreachable from their
//! command line.
//!
//! **The `*Args` structs are not `#[non_exhaustive]`.** They are built with
//! `ClickerArgs { cps: 30.0, ..ClickerArgs::new() }`, and `#[non_exhaustive]` forbids that
//! syntax across crate boundaries. The `new()` constructor already provides the
//! forward-compatibility that attribute is usually reached for. The *enums* are
//! `#[non_exhaustive]`, where it is correct.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use sequencer_core::click::{ActivationMode, ClickAction, ClickConfig};
use sequencer_core::input::Key;
use sequencer_core::time::{Duration, Period};

/// Synthetic input for Linux.
#[derive(Parser, Debug)]
#[command(
    name = "sequencer",
    version,
    about = "Synthetic input for Linux",
    subcommand_required = true,
    arg_required_else_help = true,
    propagate_version = true
)]
pub struct Cli {
    /// What to run.
    ///
    /// Required rather than defaulting to `clicker`: more modes are coming, and a tool
    /// where one of them is silently implied gets harder to read as they arrive.
    #[command(subcommand)]
    pub command: Command,
}

impl From<Command> for Cli {
    fn from(command: Command) -> Self {
        Self { command }
    }
}

/// What to do.
///
/// `#[non_exhaustive]`: a subcommand added later must not break a downstream `match`.
/// Add a `_ =>` arm, or call [`Command::run`] and never match at all.
#[derive(Subcommand, Debug)]
#[non_exhaustive]
pub enum Command {
    /// Hold or toggle a key to click, or to repeat a key press.
    Clicker(ClickerArgs),
    /// Measure how many clicks per second this machine can actually deliver.
    Bench(BenchArgs),
    /// Report what this machine can and cannot do, and how to fix what it cannot.
    Doctor(DoctorArgs),
    /// Replay a scripted list of input events through the engine and print the timeline.
    Simulate(SimulateArgs),
}

impl Command {
    /// The options every subcommand shares.
    #[must_use]
    pub const fn global(&self) -> &GlobalArgs {
        match self {
            Self::Clicker(args) => &args.global,
            Self::Bench(args) => &args.global,
            Self::Doctor(args) => &args.global,
            Self::Simulate(args) => &args.clicker.global,
        }
    }

    /// Runs this against the real backend, reporting its own diagnostics.
    ///
    /// The one-liner for a wrapping CLI: `sequencer_cli::Command::Clicker(args).run()`.
    #[must_use]
    pub fn run(self) -> u8 {
        crate::run_command(&self)
    }

    /// Runs this against the real backend, handing any failure back instead of printing.
    ///
    /// # Errors
    ///
    /// Whatever the subcommand fails with.
    pub fn try_run(self) -> crate::Result<u8> {
        crate::try_run_command(&self)
    }
}

/// Options every subcommand shares.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct GlobalArgs {
    /// Print more. Repeat for more still.
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Print nothing but errors.
    #[arg(short, long, conflicts_with = "verbose")]
    pub quiet: bool,
}

impl GlobalArgs {
    /// Every flag at the value it takes when not passed.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            verbose: 0,
            quiet: false,
        }
    }
}

impl Default for GlobalArgs {
    fn default() -> Self {
        Self::new()
    }
}

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
        long = "key",
        alias = "key_press",
        alias = "key-press",
        value_name = "KEY",
        value_parser = parse_key
    )]
    pub key: Option<Key>,

    /// Which mouse button to click, when `--key` is not given.
    #[arg(long, value_enum, default_value_t = MouseButton::Left, value_name = "BUTTON")]
    pub button: MouseButton,

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

    /// Resolve everything and report it, without touching any input device.
    #[arg(long)]
    pub dry_run: bool,
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
            key: None,
            button: MouseButton::Left,
            activate: Key::F9,
            quit: Key::F8,
            limit: 0,
            key_hold_ms: 1,
            dry_run: false,
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
            action: match self.key {
                Some(key) => ClickAction::Key(key),
                None => ClickAction::Button(self.button.into()),
            },
            activate: self.activate,
            quit: self.quit,
            limit: self.limit,
            key_hold: Duration::from_millis(self.key_hold_ms),
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

/// `sequencer bench`.
#[derive(Args, Debug, Clone, PartialEq)]
pub struct BenchArgs {
    /// Shared options.
    #[command(flatten)]
    pub global: GlobalArgs,

    /// Target rate. Omit to run flat out and find the machine's ceiling.
    #[arg(long, value_name = "RATE", value_parser = parse_cps)]
    pub cps: Option<f64>,

    /// How long to measure for.
    #[arg(long, default_value_t = 3.0, value_name = "SECONDS", value_parser = parse_seconds)]
    pub seconds: f64,
}

impl BenchArgs {
    /// Every flag at its command-line default.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            global: GlobalArgs::new(),
            cps: None,
            seconds: 3.0,
        }
    }
}

impl Default for BenchArgs {
    fn default() -> Self {
        Self::new()
    }
}

/// `sequencer doctor`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct DoctorArgs {
    /// Shared options.
    #[command(flatten)]
    pub global: GlobalArgs,
}

impl DoctorArgs {
    /// Every flag at its command-line default.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            global: GlobalArgs::new(),
        }
    }
}

impl Default for DoctorArgs {
    fn default() -> Self {
        Self::new()
    }
}

/// `sequencer simulate`.
#[derive(Args, Debug, Clone, PartialEq)]
pub struct SimulateArgs {
    // No `global` of its own: `click` already flattens `GlobalArgs`, and flattening it
    // twice into one command makes clap reject the whole definition for duplicate
    // argument names.
    /// A script of input events, one per line: `<milliseconds> <down|up> <key>`.
    ///
    /// Blank lines and `#` comments are ignored. Use `-` to read standard input.
    #[arg(value_name = "SCRIPT")]
    pub script: PathBuf,

    /// The clicker settings to replay the script against.
    #[command(flatten)]
    pub clicker: ClickerArgs,

    /// Stop after this many milliseconds of simulated time.
    #[arg(long, default_value_t = 2000, value_name = "MS")]
    pub until_ms: u64,
}

/// Rejects a bad rate at parse time, so `--cps 0` is a clean usage error rather than a
/// division by zero much later.
fn parse_cps(raw: &str) -> Result<f64, String> {
    let value: f64 = raw
        .parse()
        .map_err(|_| format!("`{raw}` is not a number"))?;
    Period::from_cps(value).map_err(|err| err.to_string())?;
    Ok(value)
}

fn parse_seconds(raw: &str) -> Result<f64, String> {
    let value: f64 = raw
        .parse()
        .map_err(|_| format!("`{raw}` is not a number"))?;
    if !value.is_finite() || value <= 0.0 {
        return Err(format!("must be a positive number of seconds, got {value}"));
    }
    Ok(value)
}

fn parse_key(raw: &str) -> Result<Key, String> {
    raw.parse::<Key>().map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory as _;

    /// The one-line test that is easy to forget. It runs clap's own consistency checks --
    /// duplicate argument ids, defaults that fail their own parser -- and without it
    /// those become panics a user hits rather than a test failure we do.
    #[test]
    fn the_cli_definition_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    fn parse(args: &[&str]) -> Command {
        Cli::try_parse_from(args).expect("should parse").command
    }

    #[test]
    fn a_subcommand_is_required() {
        // No implied default: with more modes coming, `sequencer --cps 20` silently
        // meaning "clicker" would get harder to read, not easier.
        assert!(Cli::try_parse_from(["sequencer"]).is_err());
        assert!(Cli::try_parse_from(["sequencer", "--cps", "30"]).is_err());
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
    fn bench_defaults_to_flat_out_for_three_seconds() {
        let Command::Bench(args) = parse(&["sequencer", "bench"]) else {
            panic!("expected bench");
        };
        assert_eq!(args.cps, None, "no target means find the ceiling");
        assert!((args.seconds - 3.0).abs() < f64::EPSILON);

        let Command::Bench(args) = parse(&["sequencer", "bench", "--cps", "500", "--seconds", "1"])
        else {
            panic!("expected bench");
        };
        assert_eq!(args.cps, Some(500.0));
        assert!((args.seconds - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_bad_duration_is_a_usage_error() {
        for bad in ["0", "-1", "nan", "banana"] {
            assert!(
                Cli::try_parse_from(["sequencer", "bench", "--seconds", bad]).is_err(),
                "--seconds {bad} should have been rejected"
            );
        }
    }

    #[test]
    fn the_prototypes_key_press_spelling_still_works() {
        // The Python prototype used `--key_press`; breaking that for no reason would be
        // rude to the one user who has it in their shell history.
        for spelling in ["--key", "--key_press", "--key-press"] {
            let Command::Clicker(args) = parse(&["sequencer", "clicker", spelling, "f"]) else {
                panic!("expected clicker");
            };
            assert_eq!(args.key, Some(Key::F), "{spelling}");
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
    fn verbose_and_quiet_are_mutually_exclusive() {
        assert!(Cli::try_parse_from(["sequencer", "clicker", "-v", "-q"]).is_err());
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

    #[test]
    fn every_subcommand_exposes_the_shared_options() {
        let commands = [
            Command::Clicker(ClickerArgs::new()),
            Command::Bench(BenchArgs::new()),
            Command::Doctor(DoctorArgs::new()),
        ];
        for command in commands {
            assert_eq!(command.global().verbose, 0);
        }
    }
}
