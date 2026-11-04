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
    // Checked once, up front: whether they pass decides what the backend line says.
    let checks: Vec<(Requirement, CheckResult)> = REQUIREMENTS
        .iter()
        .map(|&requirement| (requirement, requirement.check()))
        .collect();
    let devices = device_access(&checks);

    writeln!(
        deps.out,
        "sequencer {}  ({} {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    )?;
    writeln!(deps.out, "session: {}", info.session)?;
    writeln!(
        deps.out,
        "backend: {}",
        profile_backend(devices, on_x11, wayland)
    )?;
    // What Xwayland shows native Wayland apps matters only where profiles may run through
    // it: not when the devices are open to them.
    if on_x11 && wayland && devices != DeviceAccess::Open {
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
    for (requirement, result) in checks {
        match result {
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
    input_devices::write(args.global.verbose > 0, deps)?;

    write_remediations(&unmet, deps)?;

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
    if let Some(spared) = devices_spared(devices, on_x11, wayland) {
        writeln!(deps.out, "\n{spared}")?;
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

/// Whether a profile manager could hold the input devices here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeviceAccess {
    /// This build has no device backend.
    NotBuilt,
    /// This user can open them: every device requirement is met.
    Open,
    /// Not as this user, but as root: every unmet requirement is a permission, so sudo
    /// would open them.
    RootOnly,
    /// Not even as root: setup has to supply something first (the uinput node), so sudo
    /// would buy nothing — and is neither asked for nor promised.
    Missing,
}

/// [`DeviceAccess`], from the device requirements' results — sudo judged by the very
/// question a run asks before it prompts (`elevate::sudo_would_help`).
fn device_access(checks: &[(Requirement, CheckResult)]) -> DeviceAccess {
    let unmet: Vec<Requirement> = checks
        .iter()
        .filter(|(_, result)| !result.is_pass())
        .map(|&(requirement, _)| requirement)
        .collect();
    if !cfg!(feature = "evdev") {
        DeviceAccess::NotBuilt
    } else if unmet.is_empty() {
        DeviceAccess::Open
    } else if crate::elevate::sudo_would_help(&unmet) {
        DeviceAccess::RootOnly
    } else {
        DeviceAccess::Missing
    }
}

/// Why the failed device checks don't hold profiles back, where they don't: in a Wayland
/// session `profile-apply` gets the devices through sudo where root can open them, and runs
/// the profiles through Xwayland where nothing can until setup is done; in an X11 session
/// profiles run on the X server's grabs. Xwayland is named only when it is the server: said
/// on plain X11, it would be wrong. A build without the X11 backend has no profiles to
/// spare, and neither has a session with no X server where the devices need setting up.
const fn devices_spared(
    devices: DeviceAccess,
    on_x11: bool,
    wayland: bool,
) -> Option<&'static str> {
    if !cfg!(feature = "xtest") {
        return None;
    }
    match (devices, on_x11, wayland) {
        (DeviceAccess::RootOnly, true, true) => Some(
            "Profiles get the devices through sudo instead: `profile-apply` opens them as root \
             and drops root straight after, and without a terminal to ask from, runs the \
             profiles through Xwayland. The checks above are for doing without sudo — \
             `detect-key` and `bench` ask for it too.",
        ),
        (DeviceAccess::RootOnly, false, true) => Some(
            "Profiles get the devices through sudo instead: `profile-apply` opens them as root \
             and drops root straight after. The checks above are for doing without sudo — \
             `detect-key` and `bench` ask for it too.",
        ),
        (DeviceAccess::Missing, true, true) => Some(
            "Until that is fixed, profiles run through Xwayland: sudo can't supply what is \
             missing, so `profile-apply` doesn't ask for it.",
        ),
        (_, true, false) => Some(
            "Profiles need none of that: on X11 their output goes through XTEST and their \
             triggers through key grabs, and neither opens a device. The checks above are for \
             `detect-key` and `bench`, which read the devices.",
        ),
        _ => None,
    }
}

/// What `profile-apply` runs on, for `doctor` to report: the input devices where it can —
/// at once when this user can open them, through sudo in a Wayland session (the only one
/// `profile-apply` asks it in, and only from a terminal) when root can open them — and X11's
/// key grabs where it must. A declined sudo ends the run (`elevate::run_with_sudo_prompt`
/// hands back sudo's exit code), so the fallback is named for the one way to it: no
/// terminal to ask from. Where root couldn't open the devices either, sudo is not offered
/// at all. The manager is built only with the X11 backend, so without that there is
/// nothing to name.
const fn profile_backend(devices: DeviceAccess, on_x11: bool, wayland: bool) -> &'static str {
    if !cfg!(feature = "xtest") {
        return "none — profiles need the X11 backend, which this build leaves out";
    }
    match (devices, on_x11, wayland) {
        (DeviceAccess::Open, _, _) => {
            "the input devices — held, their keys passed on through a virtual keyboard"
        }
        (DeviceAccess::RootOnly, true, true) => {
            "the input devices, through sudo: `profile-apply` opens them as root and drops root \
             straight after; without a terminal, X11 through Xwayland"
        }
        (DeviceAccess::RootOnly, false, true) => {
            "the input devices, through sudo: `profile-apply` opens them as root and drops root \
             straight after"
        }
        (DeviceAccess::Missing, true, true) => {
            "X11 through Xwayland until the devices are set up (sudo can't supply what is \
             missing below) — XTEST for output, key grabs for triggers"
        }
        (_, true, true) => {
            "X11 through Xwayland — XTEST for output, key grabs for triggers (no device access, \
             no sudo)"
        }
        (_, true, false) => {
            "X11 — XTEST for output, key grabs for triggers (no device access, no sudo)"
        }
        (DeviceAccess::Missing, false, _) => {
            "none — profiles need the input devices, which need setting up first (see below), \
             or an X server, and none answered"
        }
        (DeviceAccess::RootOnly, false, false) => {
            "none — profiles need the input devices, which this user can't open, or an X \
             server, and none answered"
        }
        (DeviceAccess::NotBuilt, false, _) => "none — profiles need an X server, and none answered",
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

    /// Mouse buttons are triggers nowhere — the binds format refuses a profile that makes one
    /// a trigger — so the note says so, rather than leave the compositor's mouse sharing
    /// (KDE has a setting for it, which `kde::xwayland_sharing` doesn't read for that reason)
    /// to suggest that sharing them would make them fire.
    const MOUSE_REFUSED: &str = "- a mouse button can't be a trigger, in any session: a profile \
                                 naming one as a trigger is refused";

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
                MOUSE_REFUSED.to_owned(),
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
        vec![
            "Native Wayland apps see profiles only as far as KDE lets X11 apps:".to_owned(),
            format!(
                "- while one has focus, a trigger fires only on what KDE shares with X11 \
                 apps: {keys} (System Settings → Legacy X11 App Support)"
            ),
            MOUSE_REFUSED.to_owned(),
            "- a bound key still reaches that app, so a bind adds to the key there rather \
             than replacing it"
                .to_owned(),
            "- output asks KDE's permission once, to allow \"remote control\"".to_owned(),
        ]
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// KDE's default named in the user's words, with where to change it; the refused
        /// mouse triggers, the swallowing and the permission said once each.
        #[test]
        fn on_kde_the_note_names_the_setting_in_force() {
            let text = note(Some(XwaylandSharing::default())).join("\n");
            assert!(
                text.contains("any key pressed while Ctrl, Alt or Meta is held")
                    && text.contains("Legacy X11 App Support"),
                "{text}"
            );
            assert_eq!(text.matches("can't be a trigger").count(), 1, "{text}");
            assert!(text.contains("still reaches that app"), "{text}");
            assert!(text.contains("remote control"), "{text}");
        }

        /// Every setting KDE offers. The match makes a setting added later fail to build here
        /// until it is listed, so no test below can quietly skip it.
        fn every_setting() -> [KeySharing; 4] {
            [
                KeySharing::None,
                KeySharing::Modifiers,
                KeySharing::Combinations,
                KeySharing::All,
            ]
            .map(|keys| match keys {
                KeySharing::None
                | KeySharing::Modifiers
                | KeySharing::Combinations
                | KeySharing::All => keys,
            })
        }

        /// Mouse buttons are triggers in no session, so every note says they are refused —
        /// off KDE, and under each of KDE's settings.
        #[test]
        fn every_note_says_mouse_triggers_are_refused() {
            let kde = every_setting().map(|keys| note(Some(XwaylandSharing { keys })));
            for lines in kde.into_iter().chain([note(None)]) {
                let text = lines.join("\n");
                assert!(
                    text.contains("a mouse button can't be a trigger, in any session")
                        && text.contains("is refused"),
                    "{text}"
                );
            }
        }

        #[test]
        fn every_kde_setting_has_its_own_words() {
            let mut seen = Vec::new();
            for keys in every_setting() {
                let line = note(Some(XwaylandSharing { keys }))[1].clone();
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

/// The `input devices` section: every device the kernel lists, and what a profile manager
/// does with it — holds it, or leaves it alone and why — by the rule the grab itself applies,
/// asked of sysfs so that it needs no access to the devices. `-v` adds the devices that send
/// no keys, and gives udev's tags for each, to set beside `udevadm info`.
#[cfg(all(feature = "evdev", feature = "xtest", target_os = "linux"))]
mod input_devices {
    use sequencer_input::devices::{self, Kind};
    use sequencer_input::linux::grab::{self, Left, Take};

    use crate::{Deps, Result};

    pub(super) fn write(verbose: bool, deps: &mut Deps<'_>) -> Result<()> {
        let rows: Vec<Row> = devices::list()
            .into_iter()
            .map(|device| Row {
                take: grab::take(&device.name, &device.capabilities),
                tags: device
                    .capabilities
                    .kinds()
                    .into_iter()
                    .map(Kind::udev_tag)
                    .collect(),
                name: device.name,
                node: device.node,
            })
            .collect();
        writeln!(deps.out)?;
        for line in report(&rows, verbose) {
            writeln!(deps.out, "{line}")?;
        }
        Ok(())
    }

    /// One device, as the report needs it.
    struct Row {
        node: String,
        name: String,
        take: Take,
        /// udev's tags for it.
        tags: Vec<&'static str>,
    }

    /// The section's lines: the devices held, then those left alone and why — devices of one
    /// name, verdict and tags on one line, in the kernel's order, since one keyboard is often
    /// several devices. Those sending no keys are counted, or listed when `verbose`, which also
    /// gives each line its udev tags. Pure, so every verdict's wording is testable with no
    /// devices.
    fn report(rows: &[Row], verbose: bool) -> Vec<String> {
        if rows.is_empty() {
            return vec!["input devices: sysfs lists none".to_owned()];
        }
        let mut groups: Vec<(&Row, Vec<&str>)> = Vec::new();
        let mut quiet = 0;
        for row in rows {
            if row.take == Take::Leave(Left::NoKeys) && !verbose {
                quiet += 1;
                continue;
            }
            match groups.iter_mut().find(|(first, _)| {
                first.name == row.name && first.take == row.take && first.tags == row.tags
            }) {
                Some((_, nodes)) => nodes.push(&row.node),
                None => groups.push((row, vec![&row.node])),
            }
        }
        let (held, left): (Vec<_>, Vec<_>) = groups
            .iter()
            .partition(|(row, _)| matches!(row.take, Take::Grab { .. }));
        let mut lines = vec!["input devices — what a profile manager holds:".to_owned()];
        for (row, nodes) in held.into_iter().chain(left) {
            let (verdict, what) = match row.take {
                Take::Grab { keyboard: true } => ("held", Kind::Keyboard.to_string()),
                Take::Grab { keyboard: false } => ("held", Kind::Key.to_string()),
                Take::Leave(why) => ("left", why.to_string()),
            };
            let tags = match (verbose, row.tags.is_empty()) {
                (false, _) => String::new(),
                (true, true) => "  [no udev kind]".to_owned(),
                (true, false) => format!("  [{}]", row.tags.join(" ")),
            };
            lines.push(format!(
                "  {verdict}  {} ({}): {what}{tags}",
                row.name,
                nodes.join(", ")
            ));
        }
        if quiet > 0 {
            lines.push(format!("  {quiet} more send no keys (-v lists them)"));
        }
        if !rows
            .iter()
            .any(|row| row.take == Take::Grab { keyboard: true })
        {
            lines.push(
                "  none of them is a keyboard: a manager would hear nothing typed".to_owned(),
            );
        }
        lines
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn row(node: &str, name: &str, take: Take) -> Row {
            Row {
                node: node.to_owned(),
                name: name.to_owned(),
                take,
                tags: Vec::new(),
            }
        }

        /// A desk's worth of devices, in the kernel's order.
        fn desk() -> Vec<Row> {
            vec![
                row("event0", "Power Button", Take::Leave(Left::System)),
                row("event1", "Power Button", Take::Leave(Left::System)),
                row("event3", "Video Bus", Take::Grab { keyboard: false }),
                row(
                    "event4",
                    "Wireless Mouse",
                    Take::Leave(Left::Pointer(Kind::Mouse)),
                ),
                row("event6", "Office Keyboard", Take::Grab { keyboard: true }),
                row("event7", "Office Keyboard", Take::Leave(Left::NoKeys)),
                row("event10", "Office Keyboard", Take::Grab { keyboard: true }),
                row("event11", "Headphone Jack", Take::Leave(Left::NoKeys)),
            ]
        }

        fn line_with<'a>(lines: &'a [String], text: &str) -> (usize, &'a str) {
            lines
                .iter()
                .enumerate()
                .find(|(_, line)| line.contains(text))
                .map_or_else(
                    || panic!("no line with {text:?}: {lines:#?}"),
                    |(at, line)| (at, line.as_str()),
                )
        }

        /// What is held comes first, then what is left and why; one name's devices with one
        /// verdict share a line, in the kernel's order; the devices sending no keys are only
        /// counted.
        #[test]
        fn held_devices_come_first_and_one_name_shares_a_line() {
            let lines = report(&desk(), false);
            let (keyboard_at, keyboard) = line_with(&lines, "Office Keyboard (event6, event10)");
            assert!(
                keyboard.contains("held") && keyboard.ends_with(": a keyboard"),
                "{keyboard}"
            );
            let (_, video) = line_with(&lines, "Video Bus (event3)");
            assert!(
                video.contains("held") && video.ends_with(": keys"),
                "{video}"
            );
            let (power_at, power) = line_with(&lines, "Power Button (event0, event1)");
            assert!(
                power.contains("left") && power.contains("logind's"),
                "{power}"
            );
            assert!(keyboard_at < power_at, "held before left: {lines:#?}");
            let (_, mouse) = line_with(&lines, "Wireless Mouse (event4)");
            assert!(mouse.contains("a mouse — not held yet"), "{mouse}");
            line_with(&lines, "2 more send no keys");
            assert!(
                !lines.iter().any(|line| line.contains("Headphone Jack")),
                "{lines:#?}"
            );
            assert!(
                !lines
                    .iter()
                    .any(|line| line.contains("none of them is a keyboard")),
                "{lines:#?}"
            );
        }

        /// The layout: a heading, then one indented line per group, the verdict first.
        #[test]
        fn each_group_is_one_indented_line() {
            let lines = report(&desk(), false);
            assert_eq!(lines[0], "input devices — what a profile manager holds:");
            assert_eq!(lines[1], "  held  Video Bus (event3): keys");
        }

        /// `-v` lists the devices that send no keys too, and udev's tags for each.
        #[test]
        fn verbose_lists_every_device_with_its_udev_tags() {
            let mut rows = desk();
            rows[4].tags = vec!["ID_INPUT_KEY", "ID_INPUT_KEYBOARD"];
            rows[6].tags = vec!["ID_INPUT_KEY", "ID_INPUT_KEYBOARD"];
            let lines = report(&rows, true);
            let (_, jack) = line_with(&lines, "Headphone Jack (event11)");
            assert!(
                jack.contains("sends no keys") && jack.ends_with("[no udev kind]"),
                "{jack}"
            );
            let (_, keyboard) = line_with(&lines, "Office Keyboard (event6, event10)");
            assert!(
                keyboard.ends_with("[ID_INPUT_KEY ID_INPUT_KEYBOARD]"),
                "{keyboard}"
            );
            assert!(
                !lines.iter().any(|line| line.contains("more send no keys")),
                "{lines:#?}"
            );
        }

        /// Holding no keyboard is said: it is what made a whole manager hear nothing.
        #[test]
        fn holding_no_keyboard_is_said() {
            let rows = vec![
                row("event2", "USB Audio Device", Take::Grab { keyboard: false }),
                row(
                    "event6",
                    "Office Keyboard",
                    Take::Leave(Left::NotYet(Kind::Touchpad)),
                ),
            ];
            line_with(&report(&rows, false), "none of them is a keyboard");
        }

        #[test]
        fn a_machine_listing_no_devices_says_so() {
            assert_eq!(report(&[], false), vec!["input devices: sysfs lists none"]);
        }
    }
}

/// No profile manager on the devices in this build: nothing to report.
#[cfg(not(all(feature = "evdev", feature = "xtest", target_os = "linux")))]
mod input_devices {
    use crate::{Deps, Result};

    // The Result is not unnecessary: it is the signature the real one has, so the call
    // site stays free of `cfg`.
    #[allow(clippy::unnecessary_wraps)]
    pub(super) const fn write(_verbose: bool, _deps: &mut Deps<'_>) -> Result<()> {
        Ok(())
    }
}

/// Each unmet requirement's fix, in report order. A caution several fixes share — the cost
/// of joining the `input` group — is said once, with the first fix that needs it.
fn write_remediations(unmet: &[Requirement], deps: &mut Deps<'_>) -> Result<()> {
    let mut cautioned: Vec<&'static str> = Vec::new();
    for &requirement in unmet {
        write_remediation(requirement, &mut cautioned, deps)?;
    }
    Ok(())
}

fn write_remediation(
    requirement: Requirement,
    cautioned: &mut Vec<&'static str>,
    deps: &mut Deps<'_>,
) -> Result<()> {
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
    if let Some(caution) = fix.caution
        && !cautioned.contains(&caution)
    {
        writeln!(deps.out, "  NOTE: {caution}")?;
        cautioned.push(caution);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use sequencer_core::testutil::VirtualClock;

    use super::*;

    fn remediations(unmet: &[Requirement]) -> String {
        let clock = VirtualClock::new();
        let mut out = Vec::new();
        let mut deps = Deps::new(&mut out, &clock);
        write_remediations(unmet, &mut deps).expect("writes to a Vec");
        String::from_utf8(out).expect("utf-8")
    }

    /// Two fixes asking for the `input` group carry the same warning, said once and in
    /// full: no "see above" pointing at a warning that may not be there.
    #[test]
    fn a_shared_caution_is_said_once_with_the_first_fix_that_needs_it() {
        let both = remediations(&[Requirement::UinputNodeWritable, Requirement::EvdevReadable]);
        assert_eq!(both.matches("NOTE:").count(), 1, "{both}");
        let note = both.find("NOTE:").unwrap();
        assert!(
            note < both
                .find(Requirement::EvdevReadable.remediation().title)
                .unwrap(),
            "the warning comes with the first fix: {both}"
        );

        // Shown alone, the keyboard fix says it all itself.
        let alone = remediations(&[Requirement::EvdevReadable]);
        assert!(
            alone.contains("NOTE:") && alone.contains("keylogging"),
            "{alone}"
        );
    }

    /// Xwayland is named only where it is the X server, and sudo only where `profile-apply`
    /// asks for it: in a Wayland session. The way to Xwayland there is a run with no terminal
    /// to ask from — a declined sudo ends the run instead — so that is the condition named.
    #[cfg(feature = "xtest")]
    #[test]
    fn only_a_wayland_session_is_told_about_xwayland() {
        let note = |on_x11, wayland| {
            devices_spared(DeviceAccess::RootOnly, on_x11, wayland).unwrap_or_default()
        };
        assert!(note(true, true).contains("Xwayland") && note(true, true).contains("sudo"));
        assert!(
            note(true, true).contains("without a terminal"),
            "{}",
            note(true, true)
        );
        assert!(note(false, true).contains("sudo") && !note(false, true).contains("Xwayland"));
        assert!(!note(true, false).contains("Xwayland") && !note(true, false).contains("sudo"));
        assert_eq!(
            devices_spared(DeviceAccess::RootOnly, false, false),
            None,
            "nothing spares a console"
        );
    }

    /// Where root couldn't open the devices either — the uinput node is missing — sudo is
    /// neither promised nor offered: profiles run through Xwayland until setup is done, and
    /// with no X server nothing spares them.
    #[cfg(feature = "xtest")]
    #[test]
    fn what_sudo_cant_supply_is_never_promised_through_sudo() {
        use DeviceAccess::Missing;
        let spared = devices_spared(Missing, true, true).unwrap_or_default();
        assert!(
            spared.contains("through Xwayland") && spared.contains("sudo can't supply"),
            "{spared}"
        );
        assert!(!spared.contains("through sudo"), "{spared}");
        assert_eq!(devices_spared(Missing, false, true), None);

        let backend = profile_backend(Missing, true, true);
        assert!(
            backend.starts_with("X11 through Xwayland") && !backend.contains("through sudo"),
            "{backend}"
        );
        let nothing = profile_backend(Missing, false, true);
        assert!(
            nothing.starts_with("none") && !nothing.contains("sudo"),
            "{nothing}"
        );
    }

    /// Profiles run on the devices where this user can open them; through sudo in a Wayland
    /// session where root can, with Xwayland behind it — for a run with no terminal to ask
    /// from — where it answers; on X11's grabs elsewhere — no sudo is asked in an X11 session.
    #[cfg(feature = "xtest")]
    #[test]
    fn the_backend_line_follows_access_and_session() {
        use DeviceAccess::{NotBuilt, Open, RootOnly};
        assert!(profile_backend(Open, true, true).starts_with("the input devices —"));
        assert!(profile_backend(Open, false, false).starts_with("the input devices —"));
        let sudo = profile_backend(RootOnly, true, true);
        assert!(
            sudo.contains("through sudo")
                && sudo.contains("without a terminal, X11 through Xwayland"),
            "{sudo}"
        );
        let sudo_alone = profile_backend(RootOnly, false, true);
        assert!(
            sudo_alone.contains("through sudo") && !sudo_alone.contains("Xwayland"),
            "{sudo_alone}"
        );
        assert!(profile_backend(RootOnly, true, false).starts_with("X11 —"));
        assert!(profile_backend(NotBuilt, true, true).starts_with("X11 through Xwayland"));
        assert!(profile_backend(NotBuilt, false, false).starts_with("none"));
        assert!(profile_backend(RootOnly, false, false).starts_with("none"));
    }

    /// Without the X11 backend there are no profiles, whatever the devices allow.
    #[cfg(not(feature = "xtest"))]
    #[test]
    fn without_the_x11_backend_there_are_no_profiles_to_run() {
        assert!(profile_backend(DeviceAccess::Open, false, true).starts_with("none"));
        assert_eq!(devices_spared(DeviceAccess::RootOnly, false, true), None);
    }

    /// The devices are open to profiles only when every check passes; short of that, sudo
    /// would open them only when every unmet check is a permission — never a missing node.
    #[cfg(feature = "evdev")]
    #[test]
    fn the_devices_are_open_only_when_every_check_passes() {
        let all_pass = [
            (Requirement::UinputNodeWritable, CheckResult::Pass),
            (Requirement::EvdevReadable, CheckResult::Pass),
        ];
        assert_eq!(device_access(&all_pass), DeviceAccess::Open);
        let one_fails = [
            (Requirement::UinputNodeWritable, CheckResult::Pass),
            (
                Requirement::EvdevReadable,
                CheckResult::Fail("denied".into()),
            ),
        ];
        assert_eq!(device_access(&one_fails), DeviceAccess::RootOnly);
        let unknown = [(
            Requirement::EvdevReadable,
            CheckResult::Unknown("elsewhere"),
        )];
        assert_eq!(
            device_access(&unknown),
            DeviceAccess::RootOnly,
            "unknown is not open"
        );
        let no_node = [
            (
                Requirement::UinputModuleLoaded,
                CheckResult::Fail("/dev/uinput does not exist".into()),
            ),
            (
                Requirement::UinputNodeWritable,
                CheckResult::Fail("no such file".into()),
            ),
            (
                Requirement::EvdevReadable,
                CheckResult::Fail("denied".into()),
            ),
        ];
        assert_eq!(
            device_access(&no_node),
            DeviceAccess::Missing,
            "root can open the keyboards, but can't make the virtual one without the node"
        );
    }

    /// A build without the device backend has no devices to open, whatever is checked.
    #[cfg(not(feature = "evdev"))]
    #[test]
    fn a_build_without_the_device_backend_opens_none() {
        assert_eq!(device_access(&[]), DeviceAccess::NotBuilt);
    }
}
