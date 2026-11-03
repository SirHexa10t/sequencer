//! The doctor command: what this machine can do, and the exact fix for what it cannot.

use sequencer_input::probe::{CheckResult, Step as FixStep};
use sequencer_input::{Requirement, SessionInfo};

use crate::args::DoctorArgs;
use crate::session::{Focus, Layout, Source};
use crate::{Deps, Result, exit};

/// Whether this build has an input backend at all. Without one, every command that
/// touches input refuses.
const AVAILABLE: bool = cfg!(all(
    any(feature = "evdev", feature = "xtest"),
    target_os = "linux"
));

/// Every requirement the device backend needs, in report order.
///
/// Empty in an X11-only build: nothing there opens a device, so there is nothing to check
/// and reporting failures would be reporting on a code path that was compiled out.
#[cfg(feature = "evdev")]
const REQUIREMENTS: &[Requirement] = &[
    Requirement::UinputModuleLoaded,
    Requirement::UinputNodeWritable,
    Requirement::EvdevReadable,
];
#[cfg(not(feature = "evdev"))]
const REQUIREMENTS: &[Requirement] = &[];

/// `sequencer doctor`.
///
/// # Errors
///
/// If writing the report fails.
pub fn doctor(args: &DoctorArgs, deps: &mut Deps<'_>) -> Result<u8> {
    let info = SessionInfo::detect();
    // Probed once and reused: every call opens a fresh X connection, and two calls could
    // in principle straddle a session ending and have the report contradict itself.
    let on_x11 = x11_usable();
    let wayland = crate::session::wayland_session();

    writeln!(
        deps.out,
        "sequencer {}  ({} {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    )?;
    writeln!(deps.out, "session: {}", info.session)?;
    writeln!(deps.out, "backend: {}", profile_backend(on_x11, wayland))?;
    if on_x11 && wayland {
        xwayland::write(deps)?;
    }
    write_session_sources(on_x11, deps)?;
    // Above the requirement checks on purpose: several of the paths below return early,
    // and a stuck modifier is worth reporting even on a machine that fails those.
    keyboard::write(deps)?;
    if args.global.verbose > 0 {
        for (label, value) in [
            ("DISPLAY", info.display.as_deref()),
            ("WAYLAND_DISPLAY", info.wayland_display.as_deref()),
            ("XDG_SESSION_TYPE", info.session_type.as_deref()),
        ] {
            if let Some(value) = value {
                writeln!(deps.out, "  {label}={value}")?;
            }
        }
    }
    writeln!(deps.out)?;

    if !AVAILABLE {
        writeln!(
            deps.out,
            "[fail] no input backend: this build has none for {}. Only Linux is supported.",
            std::env::consts::OS
        )?;
        return Ok(exit::FAILURE);
    }

    let mut unmet = Vec::new();
    for &requirement in REQUIREMENTS {
        match requirement.check() {
            CheckResult::Pass => writeln!(deps.out, "[ok]   {}", requirement.label())?,
            CheckResult::Unknown(why) => {
                writeln!(deps.out, "[??]   {}: {why}", requirement.label())?;
            }
            CheckResult::Fail(detail) => {
                writeln!(deps.out, "[fail] {}: {detail}", requirement.label())?;
                unmet.push(requirement);
            }
        }
    }

    for requirement in &unmet {
        write_remediation(*requirement, deps)?;
    }

    if unmet.is_empty() {
        if REQUIREMENTS.is_empty() && !on_x11 {
            writeln!(
                deps.out,
                "[fail] no X server answered, and this build has only the X11 backend."
            )?;
            return Ok(exit::FAILURE);
        }
        writeln!(deps.out, "\nReady.")?;
        return Ok(exit::OK);
    }
    if on_x11 {
        writeln!(
            deps.out,
            "\nProfiles need none of that: on X11 their output goes through XTEST and their \
             triggers through key grabs, and neither opens a device. The checks above are \
             for `detect-key` and `bench`, which read the devices."
        )?;
        return Ok(exit::OK);
    }
    {
        writeln!(
            deps.out,
            "\nUntil that is fixed, `sequencer detect-key --no-sudo` still works: it reads \
             the terminal, not the devices."
        )?;
        Ok(exit::FAILURE)
    }
}

/// What `profile-apply` runs on, for `doctor` to report. The manager has only the X11
/// pair, so without it there is nothing to name.
const fn profile_backend(on_x11: bool, wayland: bool) -> &'static str {
    if on_x11 && wayland {
        "X11 through Xwayland — XTEST for output, key grabs for triggers (no device access, \
         no sudo)"
    } else if on_x11 {
        "X11 — XTEST for output, key grabs for triggers (no device access, no sudo)"
    } else if cfg!(feature = "xtest") {
        "none — profiles need an X server, and none answered"
    } else {
        "none — profiles need the X11 backend, which this build leaves out"
    }
}

/// Whether an X server answers, which is what profiles need — making the device checks
/// informational for them.
///
/// Asks the question the manager asks before it starts —
/// [`sequencer_input::x11::is_usable`], which connects rather than reading `$DISPLAY` — so
/// the report cannot promise profiles a session the manager would refuse.
fn x11_usable() -> bool {
    #[cfg(all(feature = "xtest", target_os = "linux"))]
    {
        sequencer_input::x11::is_usable()
    }
    #[cfg(not(all(feature = "xtest", target_os = "linux")))]
    {
        false
    }
}

/// The `focus:` and `layout:` lines: who answers the profile gates' questions — and so
/// `detect-key`'s `focus:` and `lang:` lines — and what that one can see. Silent where
/// nothing answers and no profile could run, which is a console.
fn write_session_sources(on_x11: bool, deps: &mut Deps<'_>) -> Result<()> {
    let (focus, layout) = (Focus::source(), Layout::source());
    if !on_x11 && focus.is_none() && layout.is_none() {
        return Ok(());
    }
    let focus_text = match focus {
        Some(Source::X11) => "the X server (EWMH), which sees every window",
        Some(Source::Kde) => {
            "KWin, which sees every window, Wayland and X11 (through a small script \
             sequencer loads into KWin while it needs to know, and unloads after)"
        }
        Some(Source::Xwayland) => {
            "Xwayland, which sees X11 windows only: with a Wayland app focused, \
             program-gated profiles stay dormant"
        }
        None => "nothing answers here: program-gated profiles stay dormant",
    };
    let layout_text = match layout {
        Some(Source::X11) => "the X server (XKB)",
        Some(Source::Kde) => "KDE's keyboard service, which follows every switch",
        Some(Source::Xwayland) => {
            "Xwayland (XKB), which may name only the first layout and miss switches"
        }
        None => "nothing answers here: kb_lang-gated profiles stay dormant",
    };
    writeln!(deps.out, "focus: {focus_text}")?;
    writeln!(deps.out, "layout: {layout_text}")?;
    Ok(())
}

/// What native Wayland apps see of profiles that run through Xwayland — the gaps a user
/// would otherwise discover one failed trigger at a time.
///
/// On KDE the note says exactly: KWin's own setting decides which keys reach X11 apps
/// while another app has focus, and `kwinrc` records it. Elsewhere it can only say that
/// the compositor decides.
#[cfg(all(feature = "xtest", target_os = "linux"))]
mod xwayland {
    use sequencer_input::KWinFocus;
    use sequencer_input::kde::{KeySharing, XwaylandSharing, xwayland_sharing};

    use crate::{Deps, Result};

    pub(super) fn write(deps: &mut Deps<'_>) -> Result<()> {
        let kde = KWinFocus::available().then(xwayland_sharing);
        for line in note(kde) {
            writeln!(deps.out, "  {line}")?;
        }
        Ok(())
    }

    /// The note's lines, KDE's setting given when it is known. Pure, so the wording is
    /// testable with no session.
    fn note(kde: Option<XwaylandSharing>) -> Vec<String> {
        let Some(sharing) = kde else {
            return vec![
                "Native Wayland apps see profiles only as far as the compositor lets X11 apps:"
                    .to_owned(),
                "- while one has focus, a trigger fires only on keys the compositor shares \
                 with X11 apps"
                    .to_owned(),
                "- a bound key still reaches that app, so a bind adds to the key there \
                 rather than replacing it"
                    .to_owned(),
                "- output goes through the compositor, which may ask permission first".to_owned(),
            ];
        };
        let keys = match sharing.keys {
            KeySharing::None => "no keys at all",
            KeySharing::Modifiers => {
                "only keys that type no character (F-keys, Escape, arrows, modifiers, media keys)"
            }
            KeySharing::Combinations => {
                "keys that type no character, and any key pressed while Ctrl, Alt or Meta \
                 is held"
            }
            KeySharing::All => "every key",
        };
        let mouse = if sharing.mouse {
            "- mouse-button triggers fire there too"
        } else {
            "- mouse-button triggers do not fire there: KDE shares no mouse buttons"
        };
        vec![
            "Native Wayland apps see profiles only as far as KDE lets X11 apps:".to_owned(),
            format!(
                "- while one has focus, a trigger fires only on what KDE shares with X11 \
                 apps: {keys} (System Settings → Legacy X11 App Support)"
            ),
            mouse.to_owned(),
            "- a bound key still reaches that app, so a bind adds to the key there rather \
             than replacing it"
                .to_owned(),
            "- output asks KDE's permission once, to allow \"remote control\"".to_owned(),
        ]
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// KDE's default named in the user's words, with where to change it; the mouse,
        /// the swallowing and the permission said once each.
        #[test]
        fn on_kde_the_note_names_the_setting_in_force() {
            let text = note(Some(XwaylandSharing::default())).join("\n");
            assert!(
                text.contains("any key pressed while Ctrl, Alt or Meta is held")
                    && text.contains("Legacy X11 App Support"),
                "{text}"
            );
            assert!(text.contains("KDE shares no mouse buttons"), "{text}");
            assert!(text.contains("still reaches that app"), "{text}");
            assert!(text.contains("remote control"), "{text}");
        }

        #[test]
        fn every_kde_setting_has_its_own_words() {
            let mut seen = Vec::new();
            for keys in [
                KeySharing::None,
                KeySharing::Modifiers,
                KeySharing::Combinations,
                KeySharing::All,
            ] {
                let line = note(Some(XwaylandSharing { keys, mouse: true }))[1].clone();
                assert!(
                    !seen.contains(&line),
                    "two settings, one description: {line}"
                );
                seen.push(line);
            }
        }

        /// Off KDE nothing records the policy, so the note claims none.
        #[test]
        fn elsewhere_the_note_claims_no_setting() {
            let text = note(None).join("\n");
            assert!(text.contains("the compositor shares"), "{text}");
            assert!(!text.contains("KDE"), "{text}");
        }
    }
}

/// No X11 backend: nothing runs through Xwayland, so there is nothing to note.
#[cfg(not(all(feature = "xtest", target_os = "linux")))]
mod xwayland {
    use crate::{Deps, Result};

    // The Result is not unnecessary: it is the signature the real one has, so the call
    // site stays free of `cfg`.
    #[allow(clippy::unnecessary_wraps)]
    pub(super) const fn write(_deps: &mut Deps<'_>) -> Result<()> {
        Ok(())
    }
}

/// The `keyboard:` line: what the X server believes about the keyboard right now, and
/// whether that belief is self-consistent.
///
/// X11 only, and silent elsewhere — this is the *server's* state, and off X11 there is
/// no server holding any. The question it answers is the one a user cannot answer by
/// looking: a modifier the server thinks is active with no key holding it down makes
/// every chord arrive with one modifier too many, so desktop shortcuts stop matching
/// while ordinary typing looks fine.
#[cfg(all(feature = "xtest", target_os = "linux"))]
mod keyboard {
    use sequencer_core::input::{Key, Mods};
    use sequencer_input::x11::{KeyProbe, KeyboardState};

    use crate::{Deps, Result};

    /// Reads the server and writes the report; silent when there is no server to ask.
    pub(super) fn write(deps: &mut Deps<'_>) -> Result<()> {
        let Some(probe) = KeyProbe::open() else {
            return Ok(());
        };
        let Some(state) = probe.snapshot() else {
            // Reachable if the connection dies between opening and asking. Saying so
            // beats the alternative: a diagnostic must never invent a clean bill.
            writeln!(deps.out, "keyboard: [??] the X server would not answer")?;
            return Ok(());
        };
        for line in report(&state) {
            writeln!(deps.out, "{line}")?;
        }
        Ok(())
    }

    /// The lines a snapshot deserves: the facts, then what they mean.
    ///
    /// Pure, so every verdict below is pinned by tests that need no X server.
    fn report(keyboard: &KeyboardState) -> Vec<String> {
        let stuck = keyboard.stuck();

        if stuck.is_empty() {
            let mut lines = vec![format!("keyboard: consistent — {}", summary(keyboard))];
            if !keyboard.down.is_empty() {
                lines.push(
                    "  a key listed as down with your hands off the keyboard is stuck: \
                     press and release it once"
                        .to_owned(),
                );
            }
            return lines;
        }

        let named = stuck
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let mut lines = vec![
            format!(
                "keyboard: [fail] stuck modifier: {named} — the server has it active, no key is holding it"
            ),
            format!(
                "  every chord you press arrives with {named} added, so shortcuts match nothing ({})",
                summary(keyboard)
            ),
        ];
        lines.extend(
            stuck
                .iter()
                .map(|class| format!("      $ {}", clearing(*class))),
        );
        lines.push(
            "      $ setxkbmap <your layout>   # resets locks, latches and the layout group"
                .to_owned(),
        );
        lines
    }

    /// The facts in one clause: what is active, what is down, which locks are on.
    fn summary(keyboard: &KeyboardState) -> String {
        let active = if keyboard.state.is_none() {
            "no modifier active".to_owned()
        } else {
            format!("active: {}", keyboard.state)
        };
        let down = if keyboard.down.is_empty() {
            "no key down".to_owned()
        } else {
            format!("down: {}", names(&keyboard.down))
        };
        let locks = match (keyboard.caps_lock, keyboard.num_lock) {
            (true, true) => " (caps lock and num lock ON)",
            (true, false) => " (caps lock ON)",
            (false, true) => " (num lock ON)",
            (false, false) => "",
        };
        format!("{active}, {down}{locks}")
    }

    /// How to clear a stuck class: the server is waiting to see a key come up, and only
    /// the side it is waiting on will do. Alt and AltGr are one key each, so they say so.
    fn clearing(class: Mods) -> String {
        let keys = class.watch_keys();
        if keys.len() > 1 {
            format!("tap the left and the right {class} once each — only the stuck side clears it")
        } else {
            format!("tap {} once", names(&keys))
        }
    }

    fn names(keys: &[Key]) -> String {
        keys.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn snapshot(state: Mods, down: &[Key]) -> KeyboardState {
            KeyboardState {
                state,
                down: down.to_vec(),
                caps_lock: false,
                num_lock: false,
            }
        }

        /// An idle keyboard is one line, and nothing in it sounds like a problem.
        #[test]
        fn an_idle_keyboard_reports_one_consistent_line() {
            let lines = report(&snapshot(Mods::NONE, &[]));
            assert_eq!(lines.len(), 1, "{lines:?}");
            assert!(
                lines[0].contains("consistent")
                    && lines[0].contains("no modifier active")
                    && lines[0].contains("no key down"),
                "{}",
                lines[0]
            );
            assert!(!lines[0].contains("[fail]"));
        }

        /// The whole point: a class the server has active that no key is supplying.
        /// The advice names both sides, because only the stuck one clears it.
        #[test]
        fn a_modifier_with_no_key_behind_it_is_reported_stuck() {
            let lines = report(&snapshot(Mods::SHIFT, &[]));
            let text = lines.join("\n");
            assert!(text.contains("[fail]") && text.contains("stuck modifier: shift"));
            assert!(
                text.contains("left and the right shift") && text.contains("stuck side"),
                "the fix must name both sides: {text}"
            );
            assert!(
                text.contains("setxkbmap"),
                "and the one command that resets everything: {text}"
            );
        }

        /// A held key explains its own class: fingers on the keyboard are not a fault,
        /// but a key down with no fingers is, so the line says how to tell.
        #[test]
        fn a_modifier_its_own_key_supplies_is_not_stuck() {
            let lines = report(&snapshot(Mods::SHIFT, &[Key::RightShift]));
            let text = lines.join("\n");
            assert!(!text.contains("[fail]"), "{text}");
            assert!(
                text.contains("active: shift") && text.contains("down: rshift"),
                "the facts name the side that is down: {text}"
            );
            assert!(
                text.contains("hands off the keyboard"),
                "a held key needs the how-to-tell line: {text}"
            );
        }

        /// Sided classes get "left and right"; one-key classes get the key. Ctrl held on
        /// one side still covers the class, so only the unsupplied class is reported.
        #[test]
        fn one_key_classes_are_not_told_to_tap_a_side() {
            let lines = report(&snapshot(Mods::ALT.and(Mods::RALT), &[]));
            let text = lines.join("\n");
            assert!(
                text.contains("tap alt once") && text.contains("tap ralt once"),
                "{text}"
            );
            assert!(!text.contains("left and the right"), "{text}");

            let mixed = report(&snapshot(Mods::CTRL.and(Mods::META), &[Key::LeftCtrl])).join("\n");
            assert!(
                mixed.contains("stuck modifier: meta") && !mixed.contains("ctrl —"),
                "a supplied class is not stuck: {mixed}"
            );
        }

        /// Locks are never a failure — they are a state the user may have chosen — but
        /// they are reported, because a lock is what strands a layout on the wrong group.
        #[test]
        fn locks_are_reported_without_being_faults() {
            let mut keyboard = snapshot(Mods::NONE, &[]);
            keyboard.caps_lock = true;
            let lines = report(&keyboard);
            assert_eq!(lines.len(), 1, "{lines:?}");
            assert!(lines[0].contains("caps lock ON") && !lines[0].contains("[fail]"));

            keyboard.num_lock = true;
            assert!(report(&keyboard)[0].contains("caps lock and num lock ON"));

            keyboard.caps_lock = false;
            assert!(report(&keyboard)[0].contains("num lock ON"));
        }
    }
}

/// No X11 backend in this build: there is no server state to read, so the report says
/// nothing rather than speculating.
#[cfg(not(all(feature = "xtest", target_os = "linux")))]
mod keyboard {
    use crate::{Deps, Result};

    // The Result is not unnecessary: it is the signature the real one has, so the call
    // site stays free of `cfg`.
    #[allow(clippy::unnecessary_wraps)]
    pub(super) fn write(_deps: &mut Deps<'_>) -> Result<()> {
        Ok(())
    }
}

fn write_remediation(requirement: Requirement, deps: &mut Deps<'_>) -> Result<()> {
    let fix = requirement.remediation();
    writeln!(deps.out, "\n{}", fix.title)?;
    writeln!(deps.out, "  {}", fix.why)?;
    for step in &fix.steps {
        match step {
            FixStep::Shell(command) => writeln!(deps.out, "      $ {command}")?,
            FixStep::WriteFile { path, body } => {
                writeln!(deps.out, "      write {path}:")?;
                for line in body.lines() {
                    writeln!(deps.out, "          {line}")?;
                }
            }
            FixStep::Manual(text) => writeln!(deps.out, "      {text}")?,
        }
    }
    if let Some(caution) = fix.caution {
        writeln!(deps.out, "  NOTE: {caution}")?;
    }
    Ok(())
}
