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
//! 1. **No elevation needed** — the command doesn't need the devices (`doctor`,
//!    `write-script`, `detect-key --no-sudo`, and `profile-apply` unless it is to become a
//!    Wayland session's manager — see [`wants_devices_in`]), or access is already there
//!    (group membership, running as root), or root couldn't give it either — a missing
//!    `/dev/uinput` (see [`sudo_would_help`]). Runs directly; sudo is never mentioned, and
//!    what fails, fails with the usual error naming the fix — or, for `profile-apply`, falls
//!    back to X11's grabs.
//! 2. **Elevation needed, terminal available** — explain what sudo is for and what will
//!    happen to the privilege, then re-exec this same command line under sudo. The child
//!    lands in [`drop_root_after_open`] and continues as the invoking user plus the
//!    `input` group. A sudo ticket we created is revoked afterwards; one that already
//!    existed is left exactly as found — see [`run_with_sudo_prompt`]. A declined password
//!    ends the run with sudo's own exit code: nothing falls back to an unprivileged run, so
//!    `doctor` never promises one.
//! 3. **Elevation needed, no terminal** — run unprivileged anyway; the open fails with
//!    the usual error, which names the fix (and `profile-apply` runs on X11's grabs, through
//!    Xwayland). Never block a pipeline on a password prompt.
//!
//! `bench` is covered too: it opens its device up front and calls the same drop through
//! [`sequencer_input::linux::BenchObserver::devices_open`], so a measurement runs
//! unprivileged even when the password prompt was what opened the device. So is
//! `profile-apply`: the manager-to-be opens the devices first and sheds root before it
//! writes a link or the lock.

use crate::args::{Cli, Command};
use crate::exit;

/// Runs `cli` the way the standalone binary would, adding the per-session sudo flow.
///
/// `doctor_hint` is how the *user's* command line spells the doctor command — an embedder
/// exposing it under its own name (say `mytool_doctor`) passes that, so the advice
/// printed is runnable rather than aspirational. The standalone binary passes
/// `"sequencer doctor"`.
///
/// Logging is not initialized here (an embedder must be free to own the logger);
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
/// we aren't root, and the devices aren't accessible as we stand — in a way root would fix.
fn session_needs_sudo(command: &Command) -> bool {
    wants_devices(command) && !is_root() && sudo_would_help(&platform::unmet(command))
}

/// Whether sudo would give a run what `unmet` says it lacks: something is unmet, and root
/// can meet all of it ([`sequencer_input::Requirement::root_can_meet`]). Where root can't —
/// a missing `/dev/uinput` — a password would buy nothing, so none is asked for. `doctor`
/// asks the same question, so what it promises and what a run asks can't part ways.
pub(crate) fn sudo_would_help(unmet: &[sequencer_input::Requirement]) -> bool {
    !unmet.is_empty() && unmet.iter().all(|requirement| requirement.root_can_meet())
}

/// Whether `command` will open real input devices, in this session as it stands.
///
/// Every flag of the commands that do is checked by the parser, so a command line that
/// cannot run is refused before it gets here — never after a password prompt.
fn wants_devices(command: &Command) -> bool {
    wants_devices_in(
        command,
        crate::session::wayland_session(),
        manager_running(),
    )
}

/// [`wants_devices`], with the session's two facts given: whether it is a Wayland session,
/// and whether a profile manager already runs.
fn wants_devices_in(command: &Command, wayland: bool, managed: bool) -> bool {
    match command {
        Command::Bench(_) => true,
        // Exact detection reads the devices; `--no-sudo` reads the terminal instead.
        Command::DetectKey(args) => !args.no_sudo,
        // The devices are what make a profile whole in a Wayland session, where X11 grabs
        // reach Xwayland only; in an X11 session the X server's grabs are whole already,
        // so a password would buy nothing. Nor when a manager runs: this run only hands
        // its profile over.
        Command::ProfileApply(_) => wayland && !managed,
        // `doctor` must see the machine as it really is: elevating it would blind it.
        _ => false,
    }
}

/// Whether a profile manager already holds the set, so a `profile-apply` only joins it.
fn manager_running() -> bool {
    #[cfg(all(feature = "xtest", target_os = "linux"))]
    {
        crate::profile::manager_running()
    }
    #[cfg(not(all(feature = "xtest", target_os = "linux")))]
    {
        false
    }
}

/// The device requirements `command` actually has.
///
/// `detect-key` only reads: no virtual device is created, so demanding a writable
/// /dev/uinput would ask for a password (or a kernel module) the run has no use for.
#[cfg(all(feature = "evdev", target_os = "linux"))]
fn requirements_of(command: &Command) -> &'static [sequencer_input::Requirement] {
    use sequencer_input::Requirement;
    match command {
        Command::DetectKey(_) => &[Requirement::EvdevReadable],
        _ => &[
            Requirement::UinputModuleLoaded,
            Requirement::UinputNodeWritable,
            Requirement::EvdevReadable,
        ],
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

/// What the run needs of the user's environment once it is the user again: where their
/// profile set lives, their session's kind and display, their bus, the terminal's own say
/// on colour — and the live tests' limit on what the grab takes, which is how a test
/// manager comes to hold the test's keyboard and nothing of the user's.
const CARRIED_ACROSS_SUDO: &[&str] = &[
    "HOME",
    "XDG_CONFIG_HOME",
    "SEQUENCER_CONFIG_DIR",
    "SEQUENCER_TEST_GRAB_ONLY",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_SESSION_TYPE",
    "XDG_CURRENT_DESKTOP",
    "WAYLAND_DISPLAY",
    "DISPLAY",
    "XAUTHORITY",
    "TERM",
    "COLORTERM",
    "NO_COLOR",
];

/// The `NAME=value` pairs `env` sets inside the elevated command: each of
/// [`CARRIED_ACROSS_SUDO`] that `vars` has, in that order.
fn carried_across_sudo(
    vars: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Vec<std::ffi::OsString> {
    CARRIED_ACROSS_SUDO
        .iter()
        .filter_map(|name| {
            vars(name).map(|value| {
                let mut pair = std::ffi::OsString::from(format!("{name}="));
                pair.push(value);
                pair
            })
        })
        .collect()
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
    // sudo resets the environment, but the run goes on as the user once the devices are
    // open, and needs the user's session to find: their profile set (HOME, XDG_CONFIG_HOME),
    // their session's kind and display, and its bus. `env` sets them inside the elevated
    // command — which any sudo policy that lets the user run this binary allows, where
    // `--preserve-env` needs a policy of its own.
    let carried = carried_across_sudo(|name| std::env::var_os(name));
    let status = std::process::Command::new("sudo")
        .arg("--")
        .arg("/usr/bin/env")
        .args(carried)
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
pub(crate) use platform::drop_root_after_open;
pub(crate) use platform::is_root;

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

    /// The device requirements `command` has that this process, as it stands, doesn't
    /// meet — the checks `doctor` reports, narrowed to the ones `command` needs (see
    /// `requirements_of`). Empty means the devices are usable as they are.
    pub(crate) fn unmet(command: &crate::Command) -> Vec<sequencer_input::Requirement> {
        super::requirements_of(command)
            .iter()
            .copied()
            .filter(|requirement| !requirement.check().is_pass())
            .collect()
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
        log::info!("root dropped; devices stay open uid={}", caller.uid);
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

    pub(crate) const fn unmet(_command: &crate::Command) -> Vec<sequencer_input::Requirement> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// sudo resets the environment, so the elevated run is handed what it needs of the
    /// user's: each carried variable that is set, as `NAME=value`, in the list's order —
    /// and nothing else, however much else is set.
    #[test]
    fn only_the_session_s_own_variables_cross_sudo() {
        let set = [
            ("WAYLAND_DISPLAY", "wayland-0"),
            ("HOME", "/home/someone"),
            ("PATH", "/usr/bin"),
            ("SSH_AUTH_SOCK", "/tmp/agent"),
        ];
        let carried = carried_across_sudo(|name| {
            set.iter()
                .find(|(var, _)| *var == name)
                .map(|(_, value)| (*value).into())
        });
        assert_eq!(carried, ["HOME=/home/someone", "WAYLAND_DISPLAY=wayland-0"]);
        assert!(carried_across_sudo(|_| None).is_empty());
    }

    /// A password is asked for only when it would buy the run what it lacks: something
    /// unmet, all of it root's to meet. A missing `/dev/uinput` is not, so a run lacking it
    /// is never asked — whatever else it lacks besides.
    #[test]
    fn sudo_is_asked_only_where_root_would_help() {
        use sequencer_input::Requirement::{EvdevReadable, UinputModuleLoaded, UinputNodeWritable};
        assert!(!sudo_would_help(&[]), "nothing unmet, nothing to ask");
        assert!(sudo_would_help(&[EvdevReadable]));
        assert!(sudo_would_help(&[UinputNodeWritable, EvdevReadable]));
        assert!(!sudo_would_help(&[UinputModuleLoaded]));
        assert!(
            !sudo_would_help(&[UinputModuleLoaded, UinputNodeWritable, EvdevReadable]),
            "root can open the keyboards, but can't make the virtual one without the node"
        );
    }

    /// The grab's limit crosses sudo under the name the grab reads: two spellings of one
    /// variable would let a test manager grab the user's keyboards after all.
    #[cfg(all(feature = "evdev", target_os = "linux"))]
    #[test]
    fn the_grab_limit_crosses_sudo_under_the_grabs_own_name() {
        assert!(CARRIED_ACROSS_SUDO.contains(&sequencer_input::linux::grab::GRAB_ONLY_VARIABLE));
    }

    #[test]
    fn only_a_device_touching_command_wants_devices() {
        assert!(wants_devices(
            &Command::Bench(crate::args::BenchArgs::new())
        ));
        let apply = Command::ProfileApply(crate::args::ProfileApplyArgs::new());
        assert!(
            wants_devices_in(&apply, true, false),
            "a Wayland session's manager-to-be wants the devices: they make a profile whole"
        );
        assert!(
            !wants_devices_in(&apply, false, false),
            "an X11 session's grabs are whole already; a password would buy nothing"
        );
        assert!(
            !wants_devices_in(&apply, true, true),
            "with a manager running, this run only hands its profile over"
        );

        let doctor = Command::Doctor(crate::args::DoctorArgs::new());
        assert!(
            !wants_devices(&doctor),
            "doctor reports missing access; elevating it would blind it"
        );
        assert!(
            wants_devices(&Command::DetectKey(crate::args::DetectKeyArgs::new())),
            "exact detection reads every device"
        );
        assert!(
            !wants_devices(&Command::DetectKey(crate::args::DetectKeyArgs {
                no_sudo: true,
                ..crate::args::DetectKeyArgs::new()
            })),
            "--no-sudo reads the terminal; sudo would be theatre"
        );
    }

    /// detect-key only reads, so it must not demand a writable /dev/uinput — that would
    /// ask for a password (or a kernel module) the run has no use for.
    #[cfg(all(feature = "evdev", target_os = "linux"))]
    #[test]
    fn detect_key_requires_only_the_readable_half() {
        use sequencer_input::Requirement;
        assert_eq!(
            requirements_of(&Command::DetectKey(crate::args::DetectKeyArgs::new())),
            &[Requirement::EvdevReadable]
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
        let text = session_prompt("mytool_doctor");
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        for expected in [
            "THIS RUN only",
            "dropped as soon as they are open",
            "nothing persists",
            "more convenient, less secure",
            "`mytool_doctor`",
        ] {
            assert!(
                flat.contains(expected),
                "prompt lost its promise {expected:?}:\n{text}"
            );
        }
    }
}
