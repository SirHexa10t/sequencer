//! The clicker command: hold or toggle a key to repeat a click or a key press.
//!
//! One product's worth of behaviour — its arguments ([`args`]), the banner it prints, how it
//! lowers settings into a profile, and how it reports what it sent. Everything it stands on is
//! general and stays outside: the run loop ([`crate::runtime`]), backend selection and the
//! other subcommands ([`crate::cmd`]), the engine and step IR in `sequencer_core`.
//!
//! The point of the separation is the planned second product. A scripted-sequence runner
//! should arrive as a sibling directory reusing all of the above unchanged, rather than
//! finding the shared machinery shaped around whichever command happened to exist first.

pub mod args;

pub use args::{ClickerArgs, MouseButton};

use sequencer_core::CompiledProfile;
use sequencer_core::clicker::{ActivationMode, ClickAction, ClickConfig};

use crate::cmd::run_profile;
use crate::runtime::{RunSummary, fuse_limit};
use crate::{Deps, Result, exit};

/// `sequencer clicker`.
///
/// # Errors
///
/// If the settings do not describe a runnable profile, or the input devices cannot be
/// opened.
pub fn clicker(args: &ClickerArgs, deps: &mut Deps<'_>) -> Result<u8> {
    let config = args.config();
    let profile = CompiledProfile::validate(config.to_profile()?)?;

    if !args.global.quiet {
        describe(&config, deps)?;
        writeln!(deps.out)?;
        deps.out.flush()?;
    }

    // The only keys the run binds — all an X11 grab needs to ask for, and deliberately no
    // more: a broader grab would be a keylogger.
    let hotkeys = [config.activate, config.quit];
    let summary = run_profile(
        profile,
        fuse_limit(config.cps),
        crate::runtime::cadence_of(config.cps),
        &hotkeys,
        deps,
    )?;
    report(&summary, args, deps)?;
    Ok(exit::OK)
}

/// Prints the settings in the words a user would use, so a surprising result can be
/// traced to a flag rather than guessed at.
fn describe(config: &ClickConfig, deps: &mut Deps<'_>) -> Result<()> {
    let what = match config.action {
        ClickAction::Button(button) => format!("{button} click"),
        ClickAction::Key(key) => format!("{key} key"),
    };
    let mode = match config.mode {
        ActivationMode::Hold => "HOLD",
        ActivationMode::Toggle => "TOGGLE",
    };
    let mut line = format!(
        "{what} {rate} | {mode}: {activate} | Quit: {quit}",
        rate = crate::style::key(&format!("{}/s", config.cps)),
        mode = crate::style::key(mode),
        activate = crate::style::key(&config.activate.to_string()),
        quit = crate::style::key(&config.quit.to_string()),
    );
    // Only when there is one: "no limit" is the default and saying so every run is noise.
    if config.limit > 0 {
        use std::fmt::Write as _;
        let _ = write!(
            line,
            " | Limit: {}",
            crate::style::key(&config.limit.to_string())
        );
    }
    writeln!(deps.out, "{line}")?;
    Ok(())
}

fn report(summary: &RunSummary, args: &ClickerArgs, deps: &mut Deps<'_>) -> Result<()> {
    if args.global.quiet {
        return Ok(());
    }
    // "sent", everywhere, and never "achieved": every number here is counted as this process
    // hands events to the backend. What a application ends up acting on can be lower — the
    // input stack above us may coalesce or discard, which is exactly the gap the README's
    // rate-ceiling section documents and `bench` measures. Reporting these as delivered would
    // be the tool telling a comfortable lie about the one thing it cannot see.
    match summary.sent_cps() {
        Some(rate) => writeln!(
            deps.out,
            "sent {} actions over {} repetitions at {rate:.0}/s (asked for {}/s) \
             — sent, not necessarily received.",
            summary.emitted, summary.iterations, args.cps
        )?,
        None => writeln!(
            deps.out,
            "sent {} actions over {} repetitions — sent, not necessarily received.",
            summary.emitted, summary.iterations
        )?,
    }
    if summary.slots_skipped > 0 {
        writeln!(
            deps.out,
            "{} repetitions were skipped: this machine could not keep up with {}/s. \
             The rate above is what it managed to send.",
            summary.slots_skipped, args.cps
        )?;
    }
    if summary.throttled > 0 {
        writeln!(
            deps.out,
            "{} actions were dropped by the output rate limit.",
            summary.throttled
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders the banner for `args`, with colour off (tests capture stdout).
    fn banner(args: &ClickerArgs) -> String {
        let clock = sequencer_core::testutil::VirtualClock::default();
        let mut out: Vec<u8> = Vec::new();
        let mut deps = Deps::new(&mut out, &clock);
        describe(&args.config(), &mut deps).expect("writing to a Vec cannot fail");
        String::from_utf8(out).expect("the banner is text")
    }

    /// One line, the four things worth re-reading, keys cased the way a keyboard labels them.
    #[test]
    fn the_banner_is_one_compact_line() {
        let line = banner(&ClickerArgs {
            cps: 20.0,
            ..ClickerArgs::new()
        });
        assert_eq!(
            line.trim(),
            "left click 20/s | HOLD: F9 | Quit: F8",
            "{line:?}"
        );
    }

    /// Toggle says TOGGLE, and a key action names the key rather than a button.
    #[test]
    fn the_mode_and_the_action_are_named_for_what_they_are() {
        let line = banner(&ClickerArgs {
            cps: 30.0,
            toggle: true,
            kb_key: Some("f".parse().expect("f is a key")),
            ..ClickerArgs::new()
        });
        assert_eq!(
            line.trim(),
            "f key 30/s | TOGGLE: F9 | Quit: F8",
            "{line:?}"
        );
    }

    /// A limit is stated only when there is one — "no limit" on every run is noise.
    #[test]
    fn the_limit_appears_only_when_set() {
        assert!(!banner(&ClickerArgs::new()).contains("Limit"));
        let limited = banner(&ClickerArgs {
            limit: 5,
            ..ClickerArgs::new()
        });
        assert!(limited.trim().ends_with("| Limit: 5"), "{limited:?}");
    }
}
