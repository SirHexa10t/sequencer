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

use clap::{Args, Parser, Subcommand};

use crate::clicker::args::{ClickerArgs, parse_cps};
use crate::write_script::WriteScriptArgs;
use sequencer_core::input::Key;

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
    /// Run a scripted sequence of input events. Not implemented yet.
    WriteScript(WriteScriptArgs),
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
            Self::WriteScript(args) => &args.global,
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
fn parse_seconds(raw: &str) -> Result<f64, String> {
    let value: f64 = raw
        .parse()
        .map_err(|_| format!("`{raw}` is not a number"))?;
    if !value.is_finite() || value <= 0.0 {
        return Err(format!("must be a positive number of seconds, got {value}"));
    }
    Ok(value)
}

pub(crate) fn parse_key(raw: &str) -> Result<Key, String> {
    raw.parse::<Key>().map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory as _;

    use super::*;
    use crate::clicker::ClickerArgs;

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
    fn verbose_and_quiet_are_mutually_exclusive() {
        assert!(Cli::try_parse_from(["sequencer", "clicker", "-v", "-q"]).is_err());
    }


    #[test]
    fn every_subcommand_exposes_the_shared_options() {
        // Every variant, listed by hand because `Command` cannot be enumerated — so the
        // count is asserted too. A subcommand added without a row here would otherwise slip
        // through silently, which is exactly what happened when `write-script` arrived.
        let commands = [
            Command::Clicker(ClickerArgs::new()),
            Command::Bench(BenchArgs::new()),
            Command::Doctor(DoctorArgs::new()),
            Command::Simulate(SimulateArgs {
                script: PathBuf::new(),
                clicker: ClickerArgs::new(),
                until_ms: 0,
            }),
            Command::WriteScript(WriteScriptArgs::new()),
        ];
        assert_eq!(
            commands.len(),
            Cli::command().get_subcommands().count(),
            "a subcommand exists that this test does not cover"
        );
        for command in commands {
            assert_eq!(command.global().verbose, 0);
        }
    }
}
