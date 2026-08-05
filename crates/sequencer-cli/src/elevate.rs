//! Session mode: sudo for one run, root for one moment.
//!
//! The standing setup (udev rule + `input` group, `sequencer doctor` walks through it) is
//! convenient but broad — every program the user runs gains read access to every input
//! device. [`run_with_sudo_prompt`] is the narrow alternative: when the devices are not
//! accessible and a terminal is available, ask sudo for **this run only**, and have the
//! elevated process drop root the instant the devices are open. Nothing persists after
//! exit, and no other program gains anything at any point.
//!
//! Three outcomes, decided in order:
//!
//! 1. **No elevation needed** — the command doesn't touch devices (`doctor`, `simulate`,
//!    `write-script`), access is already there (group membership, running as root), or the
//!    session is X11 and the whole run goes through the X server. Runs directly; sudo is
//!    never mentioned.
//! 2. **Elevation needed, terminal available** — explain what sudo is for and what will
//!    happen to the privilege, then re-exec this same command line under sudo. The child
//!    lands in [`drop_root_after_open`] and continues as the invoking user plus the
//!    `input` group. A sudo ticket we created is revoked afterwards; one that already
//!    existed is left exactly as found — see [`run_with_sudo_prompt`].
//! 3. **Elevation needed, no terminal** — run unprivileged anyway; the open fails with
//!    the usual error, which names the fix. Never block a pipeline on a password prompt.
//!
//! `bench` is covered too: it opens its device up front and calls the same drop through
//! [`sequencer_input::linux::BenchObserver::devices_open`], so a measurement runs
//! unprivileged even when the password prompt was what opened the device.

use crate::args::{Cli, Command};
use crate::exit;

/// Runs `cli` the way the standalone binary would, adding the per-session sudo flow.
///
/// `doctor_hint` is how the *user's* command line spells the doctor command — an embedder
/// exposing it under its own name (say `clicker_doctor`) passes that, so the advice
/// printed is runnable rather than aspirational. The standalone binary passes
/// `"sequencer doctor"`.
///
/// Logging is not initialized here (an embedder must be free to own the subscriber);
/// call [`crate::init_logging`] first if you want the run reports.
#[must_use]
pub fn run_with_sudo_prompt(cli: &Cli, doctor_hint: &str) -> u8 {
    if !session_needs_sudo(&cli.command) {
        return crate::run_cli(cli);
    }
    if !interactive() {
        // A pipeline can't answer a password prompt; fail the normal way, which prints
        // the standing-setup remediation.
        return crate::run_cli(cli);
    }
    // Politeness with a purpose: `sudo -k` after the run keeps our elevation from
    // leaking into whatever the user does next — but if they *already* held a ticket
    // (they're mid sudo-workflow), revoking would break their flow over a run they
    // weren't even prompted for. Only clear what we caused.
    let had_ticket = sudo_ticket_exists();
    if had_ticket {
        eprintln!(
            "Using your cached sudo to open the input devices; root is dropped straight after."
        );
    } else {
        eprint!("{}", session_prompt(doctor_hint));
    }
    let code = reexec_under_sudo();
    if !had_ticket {
        revoke_sudo_ticket();
    }
    code
}

/// Whether running `command` calls for the sudo round-trip: it will open input devices,
/// we aren't root, and the devices aren't accessible as we stand.
fn session_needs_sudo(command: &Command) -> bool {
    wants_devices(command) && !platform::is_root() && !platform::has_device_access()
}

/// Whether the X11 backend will handle both halves of a run — injection through XTEST and
/// hotkeys through key grabs — leaving no input device for anything to open.
///
/// Takes the answer rather than computing it, so the decision is testable: the workspace
/// denies `unsafe_code`, and `std::env::set_var` is unsafe.
///
/// The caller passes [`sequencer_input::x11::is_usable`], which *connects* rather than
/// trusting `$DISPLAY` to be set. A stale variable would otherwise convince this that no
/// password is needed, and the run would then fail on devices it never asked to open.
#[cfg(all(feature = "evdev", target_os = "linux"))]
const fn x11_handles_everything(x11_usable: bool) -> bool {
    cfg!(feature = "xtest") && x11_usable
}

/// Whether `command` will open real input devices *and* is runnable at all.
///
/// The runnability half is why settings are compiled here rather than only inside the
/// subcommand: a rate of zero, an activation key that is also the quit key — those are the
/// user's command line being wrong, and being asked for a password before being told so
/// would be insulting. An invalid command falls through to the normal path, which refuses it
/// in the usual way, unprivileged and instantly.
fn wants_devices(command: &Command) -> bool {
    match command {
        Command::Clicker(args) => args.config().to_profile().is_ok(),
        Command::Bench(_) => true,
        // `doctor` must see the machine as it really is, and `simulate` never leaves the
        // engine — elevating either would be theatre.
        _ => false,
    }
}

/// Whether there is a user at a terminal to ask.
fn interactive() -> bool {
    use std::io::IsTerminal as _;
    std::io::stdin().is_terminal()
}

/// What the user reads before sudo asks for their password. Everything they are agreeing
/// to, in order: what the elevation is for, exactly how long it lasts, and the standing
/// alternative with its own trade named.
fn session_prompt(doctor_hint: &str) -> String {
    format!(
        "No access to the input devices, so sudo opens them for THIS RUN only — root is\n\
         dropped as soon as they are open, and nothing persists after exit.\n\
         To stop being asked, `{doctor_hint}` prints a one-time setup (more convenient, less\n\
         secure: every program you run could then read every input device).\n"
    )
}

/// Re-runs this exact command line under sudo and hands back the child's exit code.
///
/// The child is the same binary with the same arguments — it re-enters wherever this
/// process did, finds itself root with `SUDO_UID` set, and [`drop_root_after_open`]
/// does the rest. No argv is reconstructed from parsed flags, so there is nothing to
/// drift when a flag is added.
fn reexec_under_sudo() -> u8 {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            eprintln!("error: could not locate this binary to re-run it under sudo: {err}");
            return exit::FAILURE;
        }
    };
    let status = std::process::Command::new("sudo")
        .arg("--")
        .arg(exe)
        .args(std::env::args_os().skip(1))
        .status();
    match status {
        Ok(status) => status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .unwrap_or(exit::FAILURE),
        Err(err) => {
            eprintln!("error: could not run sudo: {err}");
            eprintln!("for the password-free setup instead, see above or run the doctor command.");
            exit::FAILURE
        }
    }
}

/// Whether a sudo credential ticket is already cached for this user and terminal.
fn sudo_ticket_exists() -> bool {
    silent_sudo(&["-n", "true"])
}

/// Drops the cached sudo ticket this run created (`sudo -k`).
fn revoke_sudo_ticket() {
    let _ = silent_sudo(&["-k"]);
}

/// Runs `sudo <args>` with every stream silenced, reporting only success.
fn silent_sudo(args: &[&str]) -> bool {
    std::process::Command::new("sudo")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(all(feature = "evdev", target_os = "linux"))]
pub(crate) use platform::{drop_root_after_open, load_uinput_if_root};

#[cfg(all(feature = "evdev", target_os = "linux"))]
mod platform {
    use nix::unistd::{Gid, Group, Uid, geteuid, setgid, setgroups, setuid};

    use crate::{Error, Result};

    /// Who sudo says invoked us: set only when running elevated *on a user's behalf*.
    struct Caller {
        uid: Uid,
        gid: Gid,
    }

    /// The invoking user, when this process is root because of sudo. A root login proper
    /// (no `SUDO_UID`) has nobody to drop to and gets `None`.
    fn sudo_caller() -> Option<Caller> {
        if !geteuid().is_root() {
            return None;
        }
        let parse = |key: &str| std::env::var(key).ok()?.parse::<u32>().ok();
        Some(Caller {
            uid: Uid::from_raw(parse("SUDO_UID")?),
            gid: Gid::from_raw(parse("SUDO_GID")?),
        })
    }

    pub(crate) fn is_root() -> bool {
        geteuid().is_root()
    }

    /// Whether a real X server with XTEST is reachable. Always false without the feature.
    fn x11_usable() -> bool {
        #[cfg(feature = "xtest")]
        {
            sequencer_input::x11::is_usable()
        }
        #[cfg(not(feature = "xtest"))]
        {
            false
        }
    }

    /// Whether the devices are usable as this process stands — the same three checks
    /// `doctor` reports.
    pub(crate) fn has_device_access() -> bool {
        use sequencer_input::Requirement;
        // On a usable X11 session a run touches no input device at all: XTEST injects
        // through the server and the hotkeys arrive by key grab. Nothing for sudo to open,
        // so nothing to ask for — which is the whole point of that backend existing, and
        // why this returns early rather than checking anything.
        if super::x11_handles_everything(x11_usable()) {
            return true;
        }
        [
            Requirement::UinputModuleLoaded,
            Requirement::UinputNodeWritable,
            Requirement::EvdevReadable,
        ]
        .into_iter()
        .all(|requirement| requirement.check().is_pass())
    }

    /// Loads the `uinput` module if the node is missing and we can (root). Best-effort:
    /// if it still isn't there, opening the sink reports it properly.
    pub(crate) fn load_uinput_if_root() {
        if geteuid().is_root() && !std::path::Path::new("/dev/uinput").exists() {
            let _ = std::process::Command::new("modprobe")
                .arg("uinput")
                .stdin(std::process::Stdio::null())
                .status();
        }
    }

    /// The other half of session mode: called right after the devices are open. If this
    /// process is root via sudo, it becomes the invoking user again — supplementary
    /// groups reduced to `input` alone (for hot-plugged devices on distros whose nodes
    /// are group-readable), then gid, then uid, in that order because each step removes
    /// the right to perform the next.
    ///
    /// The already-open device descriptors survive the drop — permissions are checked at
    /// open, not per read — which is what makes this work even where the nodes are
    /// root-only.
    ///
    /// # Errors
    ///
    /// If any step fails, or root turns out to be regainable afterwards. Continuing as
    /// root would silently break the promise the user was given at the password prompt,
    /// so the run is refused instead.
    pub(crate) fn drop_root_after_open() -> Result<()> {
        let Some(caller) = sudo_caller() else {
            return Ok(());
        };
        let supplementary: Vec<Gid> = Group::from_name("input")
            .ok()
            .flatten()
            .map(|group| group.gid)
            .into_iter()
            .collect();
        setgroups(&supplementary)
            .and_then(|()| setgid(caller.gid))
            .and_then(|()| setuid(caller.uid))
            .map_err(|err| Error::Privilege(format!("dropping to uid {}: {err}", caller.uid)))?;
        if setuid(Uid::from_raw(0)).is_ok() {
            return Err(Error::Privilege(
                "root is still regainable after the drop; refusing to continue".into(),
            ));
        }
        tracing::info!(uid = %caller.uid, "root dropped; devices stay open");
        Ok(())
    }
}

#[cfg(not(all(feature = "evdev", target_os = "linux")))]
mod platform {
    //! Without a device backend nothing needs elevating: [`super::run_with_sudo_prompt`]
    //! falls straight through to the normal run, whose own error explains the platform.

    pub(crate) fn is_root() -> bool {
        false
    }

    pub(crate) const fn has_device_access() -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clicker::ClickerArgs;

    #[test]
    fn only_a_runnable_device_touching_command_wants_devices() {
        let real = Command::Clicker(ClickerArgs::new());
        assert!(wants_devices(&real), "a live clicker run opens devices");
        assert!(wants_devices(
            &Command::Bench(crate::args::BenchArgs::new())
        ));

        let doctor = Command::Doctor(crate::args::DoctorArgs::new());
        assert!(
            !wants_devices(&doctor),
            "doctor reports missing access; elevating it would blind it"
        );
    }

    /// The reason `x11_handles_everything` takes a probed answer rather than reading
    /// `$DISPLAY`: a variable left over from a dead session would otherwise convince this
    /// that no password is needed, and the run would then fail opening devices it had
    /// already decided it would not touch. Only a reachable X server counts.
    #[cfg(all(feature = "xtest", feature = "evdev", target_os = "linux"))]
    #[test]
    fn a_stale_display_variable_does_not_count_as_an_x11_session() {
        // `is_usable()` is what the caller passes; a false answer must put the run back on
        // the device path regardless of how the environment looks.
        assert!(
            !x11_handles_everything(false),
            "an unreachable X server must not suppress the password prompt"
        );
    }

    /// The X11 backend's whole point: a session that injects through XTEST and hears its
    /// hotkeys through key grabs opens no input device, so it must never reach the password
    /// prompt. Guarded here because it is a user-visible promise, and the check that keeps
    /// it is one `if` somebody could reasonably delete as redundant.
    #[cfg(all(feature = "xtest", feature = "evdev", target_os = "linux"))]
    #[test]
    fn an_x11_session_needs_no_device_access_and_so_no_password() {
        assert!(
            x11_handles_everything(true),
            "an X11 run touches no device: XTEST injects and grabs listen, so sudo is moot"
        );
        assert!(
            !x11_handles_everything(false),
            "with no usable X server the run is back on the devices, and may need a password"
        );
    }

    /// A command line that cannot run must be refused before a password is asked for — the
    /// whole reason settings are compiled at this layer.
    #[test]
    fn a_broken_command_line_is_refused_without_a_password_prompt() {
        let nonsense = Command::Clicker(ClickerArgs {
            cps: 0.0,
            ..ClickerArgs::new()
        });
        assert!(
            !wants_devices(&nonsense),
            "invalid settings must fall through to the normal, unprivileged refusal"
        );
    }

    /// Short, but it still has to say all four things: what sudo is for, that root is
    /// dropped, that the standing alternative exists with its trade named, and the doctor
    /// spelled the way THIS command line can run it (never this crate's own binary name,
    /// which an embedder's user does not have).
    #[test]
    fn the_password_prompt_keeps_its_promises() {
        // Matched against whitespace-collapsed text: the promises are about what the prompt
        // SAYS, and a phrase that happens to straddle a line break is still said.
        let text = session_prompt("clicker_doctor");
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        for expected in [
            "THIS RUN only",
            "dropped as soon as they are open",
            "nothing persists",
            "more convenient, less secure",
            "`clicker_doctor`",
        ] {
            assert!(
                flat.contains(expected),
                "prompt lost its promise {expected:?}:\n{text}"
            );
        }
    }
}
