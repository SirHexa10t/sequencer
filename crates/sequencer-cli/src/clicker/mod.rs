//! The clicker command: hold or toggle a key to repeat a click or a key press.
//!
//! One product's worth of behaviour — its arguments ([`args`]), the banner it prints, how it
//! lowers settings into a profile, and how it reports what it sent. Everything it stands on is
//! general and stays outside: the run loop ([`crate::runtime`]), backend selection
//! (the crate-private `backend` module), the engine and step IR in `sequencer_core`.
//!
//! The point of the separation is the planned second product. A scripted-sequence runner
//! should arrive as a sibling directory reusing all of the above unchanged, rather than
//! finding the shared machinery shaped around whichever command happened to exist first.

pub mod args;
mod config;

pub use args::{ClickerArgs, MouseButton};
pub use config::{ActivationMode, ClickAction, ClickConfig};

use sequencer_core::emit::{EmitAction, Holdable};
use sequencer_core::time::{Duration, Period};

use crate::backend::run_binds;
use crate::binds::format::{Action, Bind, Loops, Profile, Step};
use crate::runtime::{RunSummary, cadence_of};
use crate::{Deps, Error, Result, exit};

/// `sequencer clicker`.
///
/// # Errors
///
/// If the settings do not describe a runnable profile, or the input devices cannot be
/// opened.
pub fn clicker(args: &ClickerArgs, deps: &mut Deps<'_>) -> Result<u8> {
    let config = args.config();
    let profile = binds_profile(&config)?;

    if !args.global.quiet {
        describe(&config, deps)?;
        writeln!(deps.out)?;
        deps.out.flush()?;
    }

    // The only keys the run binds — all an X11 grab needs to ask for, and deliberately no
    // more: a broader grab would be a keylogger.
    let hotkeys = [config.activate, config.quit];
    let summary = run_binds(
        &profile,
        repetition_marker(&config),
        cadence_of(config.cps),
        &hotkeys,
        deps,
    )?;
    report(&summary, args, deps)?;
    Ok(exit::OK)
}

/// The clicker as a binds profile: one bind on the activation key whose sequence is a
/// single tap of the action, `emergency_stop` on the quit key, and no gate.
///
/// Exactly what a user could write in a file — and held to a file's checks through
/// [`Profile::assemble`] — which is the point: the clicker is not a second way to run a
/// repeated key, it is the first product on the same one. The mode is the `loop`: HOLD
/// is a loop that lasts while the trigger is down, TOGGLE one that lasts until the
/// trigger is pressed again; `--limit` caps either.
///
/// # Errors
///
/// A rate outside what a period can express, or a quit key that is also the activation
/// key — the format refuses both in its own words.
pub(crate) fn binds_profile(config: &ClickConfig) -> Result<Profile> {
    let period = Period::from_cps(config.cps)?.nanos();
    let (target, hold) = match config.action {
        ClickAction::Button(button) => (Holdable::Button(button), config.button_hold),
        ClickAction::Key(key) => (Holdable::Key(key), config.key_hold),
    };
    // The hold fits inside the period, half of it at most: the button has to spend a
    // while up between clicks, or at a fast rate it would never come up at all.
    let hold_nanos = u64::try_from(hold.as_nanos()).unwrap_or(u64::MAX);
    let tap_nanos = hold_nanos.min(period / 2);
    let tap = Duration::from_nanos(tap_nanos);
    let gap = Duration::from_nanos(period.saturating_sub(tap_nanos));
    let loops = match u32::try_from(config.limit) {
        Ok(0) | Err(_) => Loops::Infinite,
        Ok(1) => Loops::Once,
        Ok(times) => Loops::Times(times),
    };
    let bind = Bind {
        trigger_text: config.activate.to_string(),
        trigger: vec![config.activate],
        action: Action::Seq(vec![Step::Tap(vec![target])]),
        tap,
        gap,
        loops,
        only_while_pressed: config.mode == ActivationMode::Hold,
    };
    Profile::assemble(vec![bind], None, None, vec![vec![config.quit]]).map_err(|detail| {
        Error::Profile {
            path: "clicker".to_owned(),
            detail,
        }
    })
}

/// The action that begins each repetition — what the exit report counts.
fn repetition_marker(config: &ClickConfig) -> EmitAction {
    match config.action {
        ClickAction::Button(button) => EmitAction::ButtonDown(button),
        ClickAction::Key(key) => EmitAction::KeyDown(key),
    }
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
    // hands events to the backend. What an application ends up acting on can be lower — the
    // input stack above us may coalesce or discard, which is exactly the gap the README's
    // rate-ceiling section documents and `bench` measures. "Not all may arrive" keeps that
    // caveat on the line itself; dropping it would be the tool telling a comfortable lie
    // about the one thing it cannot see.
    match summary.sent_cps() {
        Some(rate) => writeln!(
            deps.out,
            "sent {} repetitions ({} actions) at {rate:.0}/s, asked {}/s — not all may arrive.",
            summary.iterations, summary.emitted, args.cps
        )?,
        None => writeln!(
            deps.out,
            "sent {} repetitions ({} actions) — not all may arrive.",
            summary.iterations, summary.emitted
        )?,
    }
    // A stopwatch, not a statistic: the user decides when a run starts and stops, so this
    // measures their session rather than the tool. That is also why it is wall time and
    // not `active` — a rate against it would mean nothing.
    writeln!(deps.out, "ran {:.1}s", summary.wall.as_secs_f64())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders the banner for `args`. Colour follows the process's ambient gate —
    /// libtest captures prints, not the stdout fd — so expectations are composed
    /// through the same [`crate::style`] helpers rather than written as plain text.
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
        let key = crate::style::key;
        assert_eq!(
            line.trim(),
            format!(
                "left click {} | {}: {} | Quit: {}",
                key("20/s"),
                key("HOLD"),
                key("F9"),
                key("F8")
            ),
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
        let key = crate::style::key;
        assert_eq!(
            line.trim(),
            format!(
                "f key {} | {}: {} | Quit: {}",
                key("30/s"),
                key("TOGGLE"),
                key("F9"),
                key("F8")
            ),
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
        let expected_tail = format!("| Limit: {}", crate::style::key("5"));
        assert!(limited.trim().ends_with(&expected_tail), "{limited:?}");
    }
}

#[cfg(test)]
mod lowering_tests {
    use super::*;
    use sequencer_core::input::{Button, Key};

    fn config(mode: ActivationMode, cps: f64, limit: u64) -> ClickConfig {
        ClickConfig {
            cps,
            mode,
            limit,
            ..ClickConfig::new()
        }
    }

    /// The defaults, as a profile: hold F9 for 20/s left clicks, F8 stops — one bind
    /// whose loop lasts while the trigger is down, timed so tap + gap is the period.
    #[test]
    fn the_defaults_lower_to_a_held_infinite_loop() {
        let profile = binds_profile(&ClickConfig::new()).expect("the defaults are runnable");
        assert_eq!(profile.emergency_stop, vec![vec![Key::F8]]);
        assert_eq!(profile.program, None);
        assert_eq!(profile.kb_lang, None);
        let bind = &profile.binds[0];
        assert_eq!(bind.trigger, vec![Key::F9]);
        assert_eq!(
            bind.action,
            Action::Seq(vec![Step::Tap(vec![Holdable::Button(Button::Left)])])
        );
        assert_eq!(bind.loops, Loops::Infinite);
        assert!(
            bind.only_while_pressed,
            "HOLD is a loop that lasts while pressed"
        );
        assert_eq!(bind.tap, Duration::from_millis(8));
        assert_eq!(
            bind.gap,
            Duration::from_millis(42),
            "tap + gap is the 50ms period"
        );
        assert_eq!(
            repetition_marker(&ClickConfig::new()),
            EmitAction::ButtonDown(Button::Left)
        );
    }

    /// TOGGLE is the loop that lasts until the trigger is pressed again; `--limit` caps
    /// either mode, and a limit of one is a plain run.
    #[test]
    fn toggle_and_limit_are_the_loop() {
        let toggle = binds_profile(&config(ActivationMode::Toggle, 20.0, 0)).unwrap();
        assert!(!toggle.binds[0].only_while_pressed);
        assert_eq!(toggle.binds[0].loops, Loops::Infinite);

        let capped = binds_profile(&config(ActivationMode::Hold, 20.0, 100)).unwrap();
        assert_eq!(capped.binds[0].loops, Loops::Times(100));
        assert!(
            capped.binds[0].only_while_pressed,
            "up to 100 while pressed"
        );

        let once = binds_profile(&config(ActivationMode::Toggle, 20.0, 1)).unwrap();
        assert_eq!(once.binds[0].loops, Loops::Once);
    }

    /// Every flag lands: a key action, its own hold, and the activation and quit keys.
    #[test]
    fn every_flag_lands_in_the_profile() {
        let custom = ClickConfig {
            action: ClickAction::Key(Key::F),
            key_hold: Duration::from_millis(3),
            activate: Key::F7,
            quit: Key::Escape,
            ..ClickConfig::new()
        };
        let profile = binds_profile(&custom).unwrap();
        let bind = &profile.binds[0];
        assert_eq!(bind.trigger, vec![Key::F7]);
        assert_eq!(bind.trigger_text, "F7");
        assert_eq!(profile.emergency_stop, vec![vec![Key::Escape]]);
        assert_eq!(
            bind.action,
            Action::Seq(vec![Step::Tap(vec![Holdable::Key(Key::F)])])
        );
        assert_eq!(bind.tap, Duration::from_millis(3));
        assert_eq!(repetition_marker(&custom), EmitAction::KeyDown(Key::F));
    }

    /// At a rate whose period is shorter than the hold, the hold shrinks to half the
    /// period so the button still comes up between clicks — the same rule as always.
    #[test]
    fn the_hold_shrinks_to_fit_a_fast_rate() {
        let fast = binds_profile(&config(ActivationMode::Hold, 500.0, 0)).unwrap();
        let bind = &fast.binds[0];
        assert_eq!(bind.tap, Duration::from_millis(1), "half of a 2ms period");
        assert_eq!(bind.gap, Duration::from_millis(1));
    }

    /// The quit key cannot also be the activation key — refused by the same validator
    /// a binds file goes through, in its words.
    #[test]
    fn a_quit_key_that_is_the_activation_key_is_refused() {
        let clash = ClickConfig {
            quit: Key::F9,
            ..ClickConfig::new()
        };
        let err = binds_profile(&clash).expect_err("F9 cannot both run and stop");
        assert!(err.to_string().contains("emergency_stop"), "{err}");
    }
}
