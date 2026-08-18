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
    /// Print the name of each key as it is pressed, once per press.
    DetectKey(DetectKeyArgs),
    /// Apply binds profiles: remap keys and run sequences until stopped.
    ProfileApply(ProfileApplyArgs),
    /// Remove applied profiles from the active set.
    ProfileUnapply(ProfileUnapplyArgs),
    /// Check binds files for problems without applying them.
    ProfileCheck(ProfileCheckArgs),
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
            Self::DetectKey(args) => &args.global,
            Self::ProfileApply(args) => &args.global,
            Self::ProfileUnapply(args) => &args.global,
            Self::ProfileCheck(args) => &args.global,
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

/// `sequencer detect-key`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct DetectKeyArgs {
    /// Read the terminal instead of the input devices: no sudo, works anywhere.
    ///
    /// The trade is that a terminal only knows characters — keys that type nothing
    /// (modifiers alone, media keys, mouse buttons) print nothing, and with NumLock on
    /// kp8 is indistinguishable from 8. The default reads the devices and is exact.
    #[arg(short = 'n', long)]
    pub no_sudo: bool,

    /// Shared options.
    #[command(flatten)]
    pub global: GlobalArgs,
}

impl DetectKeyArgs {
    /// Every flag at its command-line default.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            no_sudo: false,
            global: GlobalArgs::new(),
        }
    }
}

impl Default for DetectKeyArgs {
    fn default() -> Self {
        Self::new()
    }
}

/// `sequencer profile-apply`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct ProfileApplyArgs {
    /// Binds files to apply. `example_profile.toml` in the repository documents the
    /// format. A directory names every `.toml` directly inside it, in name order.
    /// Each file is linked into the active set; the first invocation becomes the
    /// manager, later ones just add to it.
    #[arg(value_name = "FILE", num_args = 1.., required = true)]
    pub files: Vec<PathBuf>,

    /// Shared options.
    #[command(flatten)]
    pub global: GlobalArgs,
}

/// `sequencer profile-check`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct ProfileCheckArgs {
    /// Binds files to check. Nothing is applied and no state is touched.
    #[arg(value_name = "FILE", num_args = 1.., required = true)]
    pub files: Vec<PathBuf>,

    /// Rewrite each sound file tidily in place, keeping every comment.
    #[arg(short, long)]
    pub format: bool,

    /// Shared options.
    #[command(flatten)]
    pub global: GlobalArgs,
}

/// `sequencer profile-unapply`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct ProfileUnapplyArgs {
    /// Profiles to remove from the active set, by name (`gaming` or `gaming.toml`).
    /// With none given, an interactive picker lists what is applied.
    #[arg(value_name = "PROFILE")]
    pub names: Vec<String>,

    /// Shared options.
    #[command(flatten)]
    pub global: GlobalArgs,
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
    fn no_sudo_parses_long_and_short() {
        for flag in ["-n", "--no-sudo"] {
            let Command::DetectKey(args) = parse(&["sequencer", "detect-key", flag]) else {
                panic!("should be detect-key");
            };
            assert!(args.no_sudo, "{flag} should set no_sudo");
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
            Command::DetectKey(DetectKeyArgs::new()),
            Command::ProfileApply(ProfileApplyArgs {
                files: Vec::new(),
                global: GlobalArgs::new(),
            }),
            Command::ProfileUnapply(ProfileUnapplyArgs {
                names: Vec::new(),
                global: GlobalArgs::new(),
            }),
            Command::ProfileCheck(ProfileCheckArgs {
                files: Vec::new(),
                format: false,
                global: GlobalArgs::new(),
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
