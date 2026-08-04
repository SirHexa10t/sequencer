//! Synthetic input for Linux: the CLI, as a library.
//!
//! Everything the `sequencer` binary does is reachable from here. The binary itself is
//! one line.
//!
//! # Using this from another program
//!
//! Three altitudes, outermost first:
//!
//! | you have | call | you get |
//! |---|---|---|
//! | raw argv | [`run`] / [`try_run`] | parse and execute; help and usage handled |
//! | a parsed [`Cli`] | [`run_cli`] / [`try_run_cli`] | execute against the real backend |
//! | a [`Command`] inside your own parser | [`Command::run`] / [`run_command`] | one subcommand |
//! | a plain config, no clap | [`run_clicker`] | the engine, nothing else |
//!
//! Every flag is a public field, and each args struct has a `new()` giving the
//! command-line defaults. Build one with struct-update syntax so a flag added in a later
//! version cannot break your build:
//!
//! ```no_run
//! use sequencer_cli::{ClickerArgs, Command};
//!
//! let args = ClickerArgs { cps: 12.5, toggle: true, ..ClickerArgs::new() };
//! let code = Command::Clicker(args).run();
//! ```
//!
//! Nesting the whole subcommand set inside your own parser works too, because
//! [`Command`] derives clap's `Subcommand`:
//!
//! ```no_run
//! use sequencer_cli::clap::{self, Parser};
//!
//! #[derive(Parser)]
//! struct MyTool {
//!     #[command(subcommand)]
//!     what: MyCommand,
//! }
//!
//! #[derive(clap::Subcommand)]
//! enum MyCommand {
//!     /// Synthetic input commands.
//!     #[command(subcommand)]
//!     Input(sequencer_cli::Command),
//! }
//!
//! let MyCommand::Input(command) = MyTool::parse().what;
//! let code = command.run();
//! ```
//!
//! # Rules this library follows so it is safe to embed
//!
//! It never calls `process::exit`, never calls `clap::Error::exit`, never reads
//! `std::env::args` for you, and never installs global state — no tracing subscriber, no
//! signal handler. [`init_logging`] exists for a `main` that wants one. Installing two
//! subscribers in one process is a panic, and it should not be this crate's decision.
//!
//! Exit codes are `u8` rather than `ExitCode` so a caller can inspect and remap them.

// Tests unwrap freely: an `unwrap()` in a test reports a failure rather than
// hiding one. Library code keeps the workspace-level `warn`.
#![cfg_attr(test, allow(clippy::unwrap_used))]

// The subcommand bodies take the clap argument structs, so they live behind the same
// gate. What remains without `cli` is the engine wiring -- `runtime`, `Deps`,
// `run_clicker` -- which is exactly what an embedder who wants the engine and not the
// command line asks for.
#[cfg(feature = "cli")]
pub mod clicker;
pub mod cmd;
pub mod runtime;
pub mod write_script;

#[cfg(feature = "cli")]
mod elevate;
mod style;
#[cfg(feature = "cli")]
pub use elevate::run_with_sudo_prompt;

#[cfg(feature = "cli")]
mod args;
mod error;

/// Re-exported so a consumer cannot end up on a different clap major than this crate.
///
/// The failure when that happens reads `expected trait clap::Args, found trait
/// clap::Args`, which is not a message anyone should have to decode. Write
/// `sequencer_cli::clap::Parser` rather than depending on clap directly.
#[cfg(feature = "cli")]
pub use clap;

pub use sequencer_core as core;
pub use sequencer_input as input;

#[cfg(feature = "cli")]
pub use crate::args::{
    BenchArgs, Cli, Command, DoctorArgs, GlobalArgs, SimulateArgs,
};
pub use crate::clicker::{ClickerArgs, MouseButton};
pub use crate::write_script::WriteScriptArgs;
pub use crate::error::Error;

use sequencer_core::clicker::ClickConfig;
use sequencer_core::emit::InputSink;
use sequencer_core::time::Clock;
use sequencer_core::{CompiledProfile, Engine};

use crate::runtime::{EventPump, RunSummary, fuse_limit, run_engine};

/// This crate's result type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Exit codes.
///
/// `2` for a usage error is what clap itself returns, and what most command-line tools
/// use.
pub mod exit {
    /// Everything worked.
    pub const OK: u8 = 0;
    /// Something went wrong at runtime.
    pub const FAILURE: u8 = 1;
    /// The command line was wrong.
    pub const USAGE: u8 = 2;
}

/// Everything a subcommand needs from outside itself.
///
/// `sink` and `pump` override backend selection when set, which is how the whole pipeline
/// gets exercised on a machine with no display server.
pub struct Deps<'a> {
    /// Where normal output goes.
    pub out: &'a mut dyn std::io::Write,
    /// The clock to schedule against.
    pub clock: &'a dyn Clock,
    /// Where synthesized input goes, if not the platform's.
    pub sink: Option<&'a mut dyn InputSink>,
    /// Where input events come from, if not the platform's.
    pub pump: Option<&'a mut dyn EventPump>,
}

impl<'a> Deps<'a> {
    /// Dependencies that use the real platform.
    pub fn new(out: &'a mut dyn std::io::Write, clock: &'a dyn Clock) -> Self {
        Self {
            out,
            clock,
            sink: None,
            pump: None,
        }
    }
}

impl std::fmt::Debug for Deps<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Deps")
            .field("sink", &self.sink.is_some())
            .field("pump", &self.pump.is_some())
            .finish_non_exhaustive()
    }
}

/// Parses `args` and runs the whole program, reporting its own diagnostics.
///
/// The batteries-included entry point: what an embedder forwarding its own command line
/// wants. Never exits the process.
#[cfg(feature = "cli")]
#[must_use]
pub fn run<I, T>(args: I) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    match try_run(args) {
        Ok(code) => code,
        Err(err) => {
            report_error(&err);
            err.exit_code()
        }
    }
}

/// Prints an error and everything underneath it.
///
/// Walking the chain matters here: the sentence a user can act on -- which permission is
/// missing, which command fixes it -- is often two levels down, and printing only the
/// outermost `Display` would hide exactly the part worth reading.
fn report_error(err: &Error) {
    let mut shown = err.to_string();
    eprintln!("error: {shown}");

    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        // Several wrappers forward `Display` to their inner error so that a caller who
        // prints only the outermost message still gets the useful sentence. That is worth
        // keeping, but it means walking the chain would otherwise repeat that sentence
        // once per layer.
        let text = cause.to_string();
        if text != shown {
            eprintln!("  caused by: {text}");
            shown = text;
        }
        source = cause.source();
    }
}

/// [`run`], but hands the failure back instead of printing it.
///
/// # Errors
///
/// Whatever the chosen subcommand fails with. A `--help` or `--version` request is not a
/// failure: it prints and yields [`exit::OK`].
#[cfg(feature = "cli")]
pub fn try_run<I, T>(args: I) -> Result<u8>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    use clap::Parser as _;

    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        // Never `err.exit()` in a library: it would take the host process down with it.
        Err(err) => {
            let _ = err.print();
            return Ok(u8::try_from(err.exit_code()).unwrap_or(exit::USAGE));
        }
    };
    try_run_cli(&cli)
}

/// Runs an already-parsed [`Cli`], reporting its own diagnostics.
#[cfg(feature = "cli")]
#[must_use]
pub fn run_cli(cli: &Cli) -> u8 {
    run_command(&cli.command)
}

/// Runs an already-parsed [`Cli`], handing the failure back.
///
/// # Errors
///
/// Whatever the chosen subcommand fails with.
#[cfg(feature = "cli")]
pub fn try_run_cli(cli: &Cli) -> Result<u8> {
    try_run_command(&cli.command)
}

/// Runs one subcommand, reporting its own diagnostics.
///
/// This is the entry point for a consumer that nested [`Command`] into its own parser and
/// so never sees our root.
#[cfg(feature = "cli")]
#[must_use]
pub fn run_command(command: &Command) -> u8 {
    match try_run_command(command) {
        Ok(code) => code,
        Err(err) => {
            report_error(&err);
            err.exit_code()
        }
    }
}

/// Runs one subcommand, handing the failure back.
///
/// # Errors
///
/// Whatever the subcommand fails with.
#[cfg(feature = "cli")]
pub fn try_run_command(command: &Command) -> Result<u8> {
    let clock = sequencer_input::SystemClock::new();
    let mut stdout = std::io::stdout().lock();
    let mut deps = Deps::new(&mut stdout, &clock);
    dispatch(command, &mut deps)
}

/// Runs one subcommand against the given dependencies.
///
/// # Errors
///
/// Whatever the subcommand fails with.
#[cfg(feature = "cli")]
pub fn dispatch(command: &Command, deps: &mut Deps<'_>) -> Result<u8> {
    match command {
        Command::Clicker(args) => clicker::clicker(args, deps),
        Command::WriteScript(args) => write_script::write_script(args, deps),
        Command::Bench(args) => cmd::bench(args, deps),
        Command::Doctor(args) => cmd::doctor(args, deps),
        Command::Simulate(args) => cmd::simulate(args, deps),
    }
}

/// Runs the clicker from a plain config, with no clap involved.
///
/// Compiles with `--no-default-features`, for an embedder that wants the engine and none
/// of the command-line surface.
///
/// # Errors
///
/// If the config does not describe a runnable profile, or the sink fails.
pub fn run_clicker(
    config: &ClickConfig,
    sink: &mut dyn InputSink,
    clock: &dyn Clock,
    pump: &mut dyn EventPump,
) -> Result<RunSummary> {
    let profile = CompiledProfile::validate(config.to_profile()?)?;
    let mut engine = Engine::new(profile, 0);
    run_engine(&mut engine, sink, clock, pump, fuse_limit(config.cps), clicker::cadence_of(config.cps))
}

/// Installs a `tracing` subscriber.
///
/// The library never calls this; `main` does. Doing it twice in one process fails, which
/// is exactly why it is not done for you.
///
/// # Errors
///
/// If a global subscriber is already installed.
#[cfg(feature = "logging")]
pub fn init_logging(verbose: u8, quiet: bool) -> Result<(), LoggingAlreadyInitialized> {
    use tracing::Level;

    // A plain level, not a filter-directive string. `tracing-subscriber`'s `env-filter`
    // would pull a whole regex engine in to parse per-module directives nobody is going
    // to write for a clicker.
    let level = if quiet {
        Level::ERROR
    } else {
        match verbose {
            0 => Level::WARN,
            1 => Level::INFO,
            2 => Level::DEBUG,
            _ => Level::TRACE,
        }
    };

    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init()
        .map_err(|_| LoggingAlreadyInitialized)
}

/// Returned when a `tracing` subscriber is already installed.
#[cfg(feature = "logging")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a tracing subscriber is already installed in this process")]
pub struct LoggingAlreadyInitialized;
